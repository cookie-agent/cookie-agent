//! The connected page's state and what it does with the daemon: the session
//! list, the viewed session's projection (the shared reducer), its
//! subscriptions, the composer, approvals, the model draft, the permission
//! mode, and usage. It mirrors the TUI, with the same calls and the same
//! shared client behaviour from `cookie_agent_client_state`; timers run on
//! the browser's event loop.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use cookie_agent_client_state::{
    ComposerInput, DeliveryOutcome, EventLevel, FullContentState, RuntimeState, StateStore,
    model_draft::{DRAFT_PUBLISH_DELAY, DraftPublish, is_foreign_model_pick},
    ownership::{SessionOwnershipOutcome, classify_session},
    subscriptions::{
        SUBSCRIPTION_RETRY_DELAY, Subscribe, SubscribeIntent, SubscriptionOutcome, Subscriptions,
    },
};
use cookie_agent_protocol::{
    AgentDescriptor, AgentMode, ApprovalListParams, ApprovalRespondParams, ApprovalStatus,
    ApprovalUserDecision, ArtifactReadParams, AvailableModelDescriptor, Client, ClientDelivery,
    ClientError, ClientResponseId, ClientRunId, EventPayload, InputDelivery, ModelSelection,
    PermissionMode, RunCancelParams, RunSelection, SessionCreateParams, SessionId,
    SessionListParams, SessionMeta, SessionOrigin, SessionPermissionGetParams,
    SessionRecallInputParams, SessionRecallInputResult, SessionSelectModelParams,
    SessionSetPermissionModeParams, SessionSubmitInputParams, SessionSubmitInputResult,
    SessionUsageParams, SessionsChangedNotification, ToolCallId, UsageRollup,
};
use leptos::{prelude::*, task::spawn_local};

use crate::{browser, connect::EVENT_LEVEL_KEY};

mod acp;
mod attachments;
mod background;
mod cards;
mod cron;
mod goal;
mod history;
mod manage;
mod nav;
mod oauth;
mod onboarding;
mod permissions;
mod projects;
mod rename;
mod restart;
mod skills;
mod tree;

pub use attachments::DownscaleOffer;
pub use cards::CardSet;
pub use cron::CronForm;
pub use history::UserMenu;
pub use manage::{ConfigEdit, MemoryChoice, MemoryEdit, MemoryPending, PANEL_POLL};
pub use onboarding::Onboarding;
pub use permissions::RuleRef;
pub use projects::{EditorFields, ProjectEditor};

/// How long usage events settle before the status bar's cost is read again.
const COST_DEBOUNCE: Duration = Duration::from_millis(250);

/// A sheet the chat view shows: on the right of a wide window, from the
/// bottom of a phone's. The daemon-wide panels are views
/// ([`crate::layout::View`]) instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Panel {
    Agents,
    Models,
    Presets,
    Approvals,
    Usage,
    Events,
    Commands,
    Goal,
    /// Compacting the context, with an optional focus.
    Compact,
    Background,
    Skills,
    Permissions,
    /// The permission mode picker.
    Modes,
    /// A user message's actions: copy, revert, fork.
    Message,
    /// The session's attached runtimes.
    Runtimes,
    /// "Move to project…" of the session in [`Workspace::moving`].
    MoveSession,
    /// The project editor ([`Workspace::project_editor`]).
    Project,
    /// The session's ACP agent options (model, mode, effort).
    AgentOptions,
}

/// Why the daemon did not create the new session, with the runtimes it was
/// asked for and the card fields they came from.
struct DraftRefusal {
    /// The error on one line, for a card.
    line: String,
    /// The error with its detail.
    message: String,
    requests: Vec<crate::cards::KeyedRequest>,
}

/// The `/usage` panel's content.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageView {
    pub session: Result<UsageRollup, String>,
    pub tree: Result<(UsageRollup, u64), String>,
}

/// The connected page's state. Every field is a handle, so the struct is
/// `Copy` and closures capture it whole.
#[derive(Clone, Copy)]
pub struct Workspace {
    client: StoredValue<Option<Client>>,
    /// The shared reducer's projections; `revision` changes whenever it does.
    store: StoredValue<StateStore>,
    pub revision: RwSignal<u64>,
    runtime: StoredValue<RuntimeState>,
    pub runtime_revision: RwSignal<u64>,
    /// Notified whenever the user sends a message (or steers or queues one):
    /// the transcript jumps to its end and follows it again.
    pub sent: Trigger,
    pub sessions: RwSignal<Vec<SessionMeta>>,
    pub selected: RwSignal<Option<SessionId>>,
    /// Composing a session that does not exist yet: the first message
    /// creates it.
    pub new_session: RwSignal<bool>,
    /// The daemon lists sessions' attachments (client protocol 1.7), so a
    /// session listing none has none.
    pub lists_attachments: RwSignal<bool>,
    /// The runtime cards the new session is created with, shown above
    /// its composer: `global` alone, or its project's defaults, until the
    /// user changes them.
    pub selection: RwSignal<crate::cards::Selection>,
    /// Why a card's runtime refused the new session, by card key and field.
    pub card_errors: RwSignal<HashMap<(String, usize), String>>,
    /// The first message is creating the new session.
    pub creating: RwSignal<bool>,
    /// Why the first message did not create the new session, beside the
    /// composer.
    pub draft_error: RwSignal<Option<String>>,
    /// The machine groups the user opened or closed now, by machine; the
    /// browser remembers them too.
    pub group_open: RwSignal<HashMap<String, bool>>,
    /// The session list shows only sessions of this directory
    /// (`cwd_identity`); `None` for all.
    pub directory_filter: RwSignal<Option<String>>,
    owned: StoredValue<HashSet<SessionId>>,
    pub read_only: RwSignal<HashSet<SessionId>>,
    ownership_classifications: StoredValue<HashMap<SessionId, u64>>,
    next_ownership_classification: StoredValue<u64>,
    subscriptions: StoredValue<Subscriptions>,
    pub draft: RwSignal<Option<RunSelection>>,
    pub preset: RwSignal<Option<String>>,
    draft_publish: StoredValue<DraftPublish>,
    pub permission_modes: RwSignal<HashMap<SessionId, PermissionMode>>,
    pub new_session_mode: RwSignal<Option<PermissionMode>>,
    pub usage: RwSignal<Option<UsageView>>,
    pub costs: RwSignal<HashMap<SessionId, f64>>,
    cost_debounce: StoredValue<HashMap<SessionId, u64>>,
    pub level: RwSignal<EventLevel>,
    /// What the user opened in each session's transcript: tool calls by
    /// ID, other rows by their block key. It outlives a switch to another
    /// session, not a reload.
    pub expanded: RwSignal<HashMap<SessionId, HashSet<String>>>,
    pub composer: RwSignal<String>,
    pub status: RwSignal<String>,
    pub panel: RwSignal<Option<Panel>>,
    /// The viewed session's tree (`session.tree` of its root).
    pub tree: RwSignal<Option<cookie_agent_protocol::SessionTree>>,
    pub tree_root: RwSignal<Option<SessionId>>,
    /// Whether the open session's tree shows every subagent, not only
    /// the running ones.
    pub tree_expanded: RwSignal<bool>,
    pub collapsed: RwSignal<HashSet<SessionId>>,
    tree_request: StoredValue<u64>,
    pub user_menu: RwSignal<Option<UserMenu>>,
    pub goal_draft: RwSignal<String>,
    pub skills: RwSignal<Option<Result<Vec<cookie_agent_protocol::SkillDescriptor>, String>>>,
    pub skill_query: RwSignal<String>,
    pub skill_pick: RwSignal<Option<cookie_agent_protocol::SkillDescriptor>>,
    pub skill_args: RwSignal<String>,
    /// The `/permissions` table's rules: the viewed session's, or the new
    /// session draft's preview.
    pub permissions:
        RwSignal<Option<Result<Vec<cookie_agent_protocol::EffectivePermissionAction>, String>>>,
    /// The rules a new session is created with.
    pub draft_rules: RwSignal<Vec<cookie_agent_protocol::PermissionRule>>,
    permission_request: StoredValue<u64>,
    /// The files the next message attaches.
    pub attachments: RwSignal<Vec<cookie_agent_protocol::ArtifactRef>>,
    /// The bytes of a new session's files, uploaded once its first message
    /// created it.
    draft_files: StoredValue<Vec<attachments::DraftFile>>,
    next_draft_file: StoredValue<u64>,
    pub downscale_offer: RwSignal<Option<DownscaleOffer>>,
    offer_bytes: StoredValue<Vec<u8>>,
    /// `runtimes.list`: read on connect and whenever runtimes or machines
    /// change, for the runtime cards and every runtime's home.
    pub runtimes: RwSignal<Option<cookie_agent_protocol::RuntimesListResult>>,
    pub machines: RwSignal<Option<Result<cookie_agent_protocol::MachinesListResult, String>>>,
    pub mcp_servers: RwSignal<Option<Result<Vec<cookie_agent_protocol::McpServerInfo>, String>>>,
    /// The daemon takes the page's own address as the OAuth redirect
    /// (client protocol 1.11), so the MCP servers offer sign-in.
    pub can_sign_in: RwSignal<bool>,
    /// The sign-ins this browser started that still wait, as
    /// `(state, server)`.
    pub sign_ins: RwSignal<Vec<(String, String)>>,
    /// How the last sign-in ended: `Ok` with the server signed in to, or
    /// the error.
    pub sign_in_outcome: RwSignal<Option<Result<String, String>>>,
    /// A callback to finish once the page is connected: the server, the
    /// address, and the sign-in's state.
    sign_in_waiting: StoredValue<Option<(String, String, Option<String>)>>,
    pub provider_operations: RwSignal<
        HashMap<
            cookie_agent_protocol::ProviderId,
            cookie_agent_client_state::providers::ProviderOperation,
        >,
    >,
    pub config: RwSignal<Option<Result<cookie_agent_protocol::ConfigGetResult, String>>>,
    /// Edits the daemon refused, by path, to be fixed and applied again.
    pub config_drafts: RwSignal<HashMap<String, ConfigEdit>>,
    pub config_error: RwSignal<Option<String>>,
    /// The document open in the editor.
    pub config_edit: RwSignal<Option<ConfigEdit>>,
    pub memory: RwSignal<Option<Result<cookie_agent_protocol::MemoryListResult, String>>>,
    /// The memory open in the editor.
    pub memory_edit: RwSignal<Option<MemoryEdit>>,
    /// A write that was not saved, waiting for the user's choice.
    pub memory_pending: RwSignal<Option<MemoryPending>>,
    /// The memory a transcript line opened the view at: shown, and marked.
    pub memory_focus: RwSignal<Option<String>>,
    pub plugins: RwSignal<Option<Result<Vec<cookie_agent_protocol::PluginInfo>, String>>>,
    /// Whether the daemon keeps cron jobs (client protocol 1.15).
    pub has_crons: RwSignal<bool>,
    /// `crons.list`: read on connect and on every `crons.changed`.
    pub crons: RwSignal<Option<Result<cookie_agent_protocol::CronsListResult, String>>>,
    /// The job the Scheduled prompts view shows the details of.
    pub cron_selected: RwSignal<Option<cookie_agent_protocol::CronJobId>>,
    /// The job form, while it is open.
    pub cron_form: RwSignal<Option<CronForm>>,
    /// Why the form was not sent, or a job action failed.
    pub cron_error: RwSignal<Option<String>>,
    /// The daemon's client protocol has `session.delete`.
    pub can_delete: RwSignal<bool>,
    /// A delete the daemon refused: the root session and its message.
    pub delete_error: RwSignal<Option<(SessionId, String)>>,
    /// The title bar's title field, while it is open: what it holds.
    pub title_edit: RwSignal<Option<String>>,
    /// A rename the daemon refused: the session and its message.
    pub rename_error: RwSignal<Option<(SessionId, String)>>,
    /// The view the rail shows.
    pub view: RwSignal<crate::layout::View>,
    /// The Settings section open; `None` for the section list on a phone,
    /// the first section on a wider window.
    pub settings_section: RwSignal<Option<crate::layout::SettingsSection>>,
    /// The sessions over the page: a phone's drawer, or the sidebar sliding
    /// over a tablet's transcript.
    pub sidebar_open: RwSignal<bool>,
    /// A desktop window shows the sessions sidebar; kept in the browser.
    pub sidebar_pinned: RwSignal<bool>,
    /// The window is phone-sized.
    pub narrow: RwSignal<bool>,
    /// Whether the daemon keeps projects (client protocol 1.8).
    pub has_projects: RwSignal<bool>,
    /// `projects.list`, in name order.
    pub projects: RwSignal<Vec<cookie_agent_protocol::ProjectInfo>>,
    /// The project sections folded or opened on this page, by section key;
    /// the browser keeps them.
    pub sections_open: RwSignal<HashMap<String, bool>>,
    /// The project the new session is created in.
    pub new_project: RwSignal<Option<cookie_agent_protocol::ProjectId>>,
    /// The session "Move to project…" moves.
    pub moving: RwSignal<Option<SessionId>>,
    /// The project editor, while it is open.
    pub project_editor: RwSignal<Option<ProjectEditor>>,
    /// The project editor's runtime cards.
    pub editor_selection: RwSignal<crate::cards::Selection>,
    /// A project change the daemon refused, under its header: the
    /// section key and the message.
    pub project_error: RwSignal<Option<(String, String)>>,
    /// The layers the browser's history holds an entry for, innermost last.
    history: StoredValue<Vec<crate::layout::Layer>>,
    /// The page went back in the history and waits for the browser to get
    /// there.
    traversing: StoredValue<bool>,
    /// Bumped by every connection the page adopts, so work for an older one
    /// stops.
    connection: StoredValue<u64>,
    /// The daemon restarts from a client (client protocol 1.19).
    pub can_restart: RwSignal<bool>,
    /// The daemon's configuration status, for the restart banner.
    pub config_status: RwSignal<Option<cookie_agent_protocol::ConfigStatus>>,
    /// The restart confirmation is open.
    pub restart_confirm: RwSignal<bool>,
    /// A restart the daemon scheduled, as the page says it, until the
    /// connection closes.
    pub restarting: RwSignal<Option<String>>,
    /// The first-run sheet and the daemon's onboarding answer.
    pub onboarding: Onboarding,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            client: StoredValue::new(None),
            store: StoredValue::new(StateStore::default()),
            revision: RwSignal::new(0),
            runtime: StoredValue::new(RuntimeState::default()),
            runtime_revision: RwSignal::new(0),
            sent: Trigger::new(),
            sessions: RwSignal::new(Vec::new()),
            selected: RwSignal::new(None),
            new_session: RwSignal::new(true),
            lists_attachments: RwSignal::new(false),
            selection: RwSignal::new(crate::cards::Selection::global_only()),
            card_errors: RwSignal::new(HashMap::new()),
            creating: RwSignal::new(false),
            draft_error: RwSignal::new(None),
            group_open: RwSignal::new(HashMap::new()),
            directory_filter: RwSignal::new(None),
            owned: StoredValue::new(HashSet::new()),
            read_only: RwSignal::new(HashSet::new()),
            ownership_classifications: StoredValue::new(HashMap::new()),
            next_ownership_classification: StoredValue::new(0),
            subscriptions: StoredValue::new(Subscriptions::default()),
            draft: RwSignal::new(None),
            preset: RwSignal::new(None),
            draft_publish: StoredValue::new(DraftPublish::default()),
            permission_modes: RwSignal::new(HashMap::new()),
            new_session_mode: RwSignal::new(None),
            usage: RwSignal::new(None),
            costs: RwSignal::new(HashMap::new()),
            cost_debounce: StoredValue::new(HashMap::new()),
            level: RwSignal::new(level_from_storage(
                browser::load(EVENT_LEVEL_KEY).as_deref(),
            )),
            expanded: RwSignal::new(HashMap::new()),
            composer: RwSignal::new(String::new()),
            status: RwSignal::new(String::new()),
            panel: RwSignal::new(None),
            tree: RwSignal::new(None),
            tree_root: RwSignal::new(None),
            tree_expanded: RwSignal::new(false),
            collapsed: RwSignal::new(HashSet::new()),
            tree_request: StoredValue::new(0),
            user_menu: RwSignal::new(None),
            goal_draft: RwSignal::new(String::new()),
            skills: RwSignal::new(None),
            skill_query: RwSignal::new(String::new()),
            skill_pick: RwSignal::new(None),
            skill_args: RwSignal::new(String::new()),
            permissions: RwSignal::new(None),
            draft_rules: RwSignal::new(Vec::new()),
            permission_request: StoredValue::new(0),
            attachments: RwSignal::new(Vec::new()),
            draft_files: StoredValue::new(Vec::new()),
            next_draft_file: StoredValue::new(0),
            downscale_offer: RwSignal::new(None),
            offer_bytes: StoredValue::new(Vec::new()),
            runtimes: RwSignal::new(None),
            machines: RwSignal::new(None),
            mcp_servers: RwSignal::new(None),
            can_sign_in: RwSignal::new(false),
            sign_ins: RwSignal::new(crate::oauth::waiting()),
            sign_in_outcome: RwSignal::new(None),
            sign_in_waiting: StoredValue::new(None),
            provider_operations: RwSignal::new(HashMap::new()),
            config: RwSignal::new(None),
            config_drafts: RwSignal::new(HashMap::new()),
            config_error: RwSignal::new(None),
            config_edit: RwSignal::new(None),
            memory: RwSignal::new(None),
            memory_edit: RwSignal::new(None),
            memory_pending: RwSignal::new(None),
            memory_focus: RwSignal::new(None),
            plugins: RwSignal::new(None),
            has_crons: RwSignal::new(false),
            crons: RwSignal::new(None),
            cron_selected: RwSignal::new(None),
            cron_form: RwSignal::new(None),
            cron_error: RwSignal::new(None),
            can_delete: RwSignal::new(false),
            delete_error: RwSignal::new(None),
            title_edit: RwSignal::new(None),
            rename_error: RwSignal::new(None),
            view: RwSignal::new(crate::layout::View::Chat),
            settings_section: RwSignal::new(None),
            sidebar_open: RwSignal::new(false),
            sidebar_pinned: RwSignal::new(crate::connect::sidebar_shown(
                browser::load(crate::connect::SIDEBAR_KEY).as_deref(),
            )),
            narrow: RwSignal::new(browser::matches(browser::PHONE_QUERY)),
            has_projects: RwSignal::new(false),
            projects: RwSignal::new(Vec::new()),
            sections_open: RwSignal::new(HashMap::new()),
            new_project: RwSignal::new(None),
            moving: RwSignal::new(None),
            project_editor: RwSignal::new(None),
            editor_selection: RwSignal::new(crate::cards::Selection::default()),
            project_error: RwSignal::new(None),
            history: StoredValue::new(Vec::new()),
            traversing: StoredValue::new(false),
            connection: StoredValue::new(0),
            can_restart: RwSignal::new(false),
            config_status: RwSignal::new(None),
            restart_confirm: RwSignal::new(false),
            restarting: RwSignal::new(None),
            onboarding: Onboarding::default(),
        }
    }

    fn client(self) -> Option<Client> {
        self.client.get_value()
    }

    /// Reads the projections; tracks `revision` when called reactively.
    pub fn with_store<T>(self, read: impl FnOnce(&StateStore) -> T) -> T {
        self.revision.track();
        self.store.with_value(read)
    }

    fn update_store<T>(self, update: impl FnOnce(&mut StateStore) -> T) -> T {
        let result = self
            .store
            .try_update_value(update)
            .expect("the store lives");
        self.revision
            .update(|revision| *revision = revision.wrapping_add(1));
        result
    }

    /// Reads the runtime snapshot; tracks `runtime_revision`.
    pub fn with_runtime<T>(self, read: impl FnOnce(&RuntimeState) -> T) -> T {
        self.runtime_revision.track();
        self.runtime.with_value(read)
    }

    fn owned(self, session_id: SessionId) -> bool {
        self.owned.with_value(|owned| owned.contains(&session_id))
    }

    fn is_current(self, connection: u64) -> bool {
        self.connection.get_value() == connection
    }

    /// Adopts a connection: the first one, or a redialed one after a drop.
    /// A redial rebuilds what the old connection fed, as the TUI does: the
    /// viewed session from a full replay, the lists, approvals, and mode.
    pub fn adopt(self, client: Client) {
        let reconnected = self.client.with_value(Option::is_some);
        self.connection.update_value(|connection| *connection += 1);
        self.can_delete.set(
            cookie_agent_client_state::session_delete::daemon_can_delete_sessions(
                client.daemon_version(),
            ),
        );
        self.lists_attachments.set(
            cookie_agent_client_state::paths::daemon_reports_runtime_homes(client.daemon_version()),
        );
        self.has_projects
            .set(cookie_agent_client_state::projects::daemon_has_projects(
                client.daemon_version(),
            ));
        self.can_sign_in.set(
            cookie_agent_client_state::mcp::daemon_accepts_page_redirects(client.daemon_version()),
        );
        self.has_crons
            .set(cookie_agent_client_state::cron::daemon_has_crons(
                client.daemon_version(),
            ));
        self.can_restart
            .set(cookie_agent_client_state::restart::daemon_can_restart(
                client.daemon_version(),
            ));
        self.restarting.set(None);
        self.restart_confirm.set(false);
        self.client.set_value(Some(client));
        self.refresh_config_status();
        self.subscriptions
            .update_value(|subscriptions| subscriptions.reset());
        self.finish_waiting_sign_in();
        let selected = self.selected.get_untracked();
        self.refresh_onboarding(!reconnected);
        spawn_local(async move {
            self.refresh_lists().await;
            if reconnected && let Some(session_id) = selected {
                let owned = self.owned(session_id);
                let subscribe = self
                    .subscriptions
                    .try_update_value(|subscriptions| {
                        subscriptions.subscribe_from_scratch(session_id, owned)
                    })
                    .expect("the subscriptions live");
                self.send_subscribe(subscribe);
                self.refresh_approvals(session_id);
                self.load_permission_mode(session_id);
                self.status.set("reconnected to the daemon".into());
            }
        });
    }

    /// The connection dropped: nothing is subscribed on it any more.
    pub fn disconnected(self) {
        self.subscriptions
            .update_value(|subscriptions| subscriptions.reset());
    }

    /// Reads the session list and the runtime snapshot again.
    pub async fn refresh_lists(self) {
        let Some(client) = self.client() else {
            return;
        };
        match client.list_sessions(SessionListParams::default()).await {
            Ok(result) => self.sessions.set(result.sessions),
            Err(error) => self.status.set(crate::errors::describe(&error)),
        }
        self.refresh_projects();
        self.poll_runtimes();
        self.poll_crons();
        match client.runtime_snapshot().await {
            Ok(result) => {
                self.runtime
                    .update_value(|runtime| runtime.install_initial(result.snapshot));
                self.runtime_revision.update(|revision| *revision += 1);
                if self.draft.get_untracked().is_none() {
                    self.draft.set(self.default_draft());
                }
            }
            Err(error) => self.status.set(format!(
                "runtime snapshot unavailable: {}",
                crate::errors::describe(&error)
            )),
        }
    }

    /// Applies one delivery of the connection's stream.
    pub fn handle(self, delivery: ClientDelivery) {
        match &delivery {
            ClientDelivery::SessionsChanged(change) => {
                self.apply_sessions_change((**change).clone());
                return;
            }
            ClientDelivery::RuntimeChanged(changed) => {
                let applied = self
                    .runtime
                    .try_update_value(|runtime| {
                        let duplicate = runtime.revision() == Some(&changed.snapshot.revision);
                        runtime.apply_notification((**changed).clone()) || duplicate
                    })
                    .unwrap_or(true);
                self.runtime_revision.update(|revision| *revision += 1);
                if !applied {
                    // Outside the predecessor chain: fetch the current one.
                    self.resync_runtime();
                }
                self.config_may_have_changed(&changed.reasons);
                return;
            }
            ClientDelivery::RecoveryFailed { session_id, error } => {
                self.subscriptions
                    .update_value(|subscriptions| subscriptions.recovery_failed(*session_id));
                self.schedule_subscription_retry(*session_id);
                self.status.set(format!("recovery failed: {error}"));
                return;
            }
            ClientDelivery::ProjectsChanged(_) => {
                self.refresh_projects();
                return;
            }
            ClientDelivery::CronsChanged(_) => {
                self.poll_crons();
                return;
            }
            ClientDelivery::DaemonRestarting(restarting) => {
                self.note_restarting(restarting);
                return;
            }
            ClientDelivery::OnboardingChanged(status) => {
                self.apply_onboarding_change(status.clone());
                return;
            }
            ClientDelivery::PluginEvent(event) => {
                if self.selected.get_untracked() == Some(event.session_id) {
                    self.status
                        .set(cookie_agent_client_state::plugins::plugin_event_status(
                            event,
                        ));
                }
                return;
            }
            ClientDelivery::MemoryChanged(_) if self.has_projects.get_untracked() => {
                // A project's memory count follows its memories.
                self.refresh_projects();
                self.refresh_open_panel(&delivery);
                return;
            }
            ClientDelivery::McpChanged(_)
            | ClientDelivery::RuntimesChanged(_)
            | ClientDelivery::PluginsChanged(_)
            | ClientDelivery::MachinesChanged(_)
            | ClientDelivery::MemoryChanged(_) => {
                // The cards and every path's home follow the runtimes.
                if matches!(
                    delivery,
                    ClientDelivery::RuntimesChanged(_) | ClientDelivery::MachinesChanged(_)
                ) && self.panel.get_untracked() != Some(Panel::Runtimes)
                {
                    self.poll_runtimes();
                }
                // An open panel refreshes now instead of at its next poll.
                self.refresh_open_panel(&delivery);
                return;
            }
            ClientDelivery::Disconnected { .. } | ClientDelivery::Diagnostic { .. } => return,
            _ => {}
        }
        let own_origin = self.client().and_then(|client| client.origin());
        let selected = self.selected.get_untracked();
        let foreign_pick = is_foreign_model_pick(&delivery, selected, own_origin.as_ref());
        // What the rest needs to know of the event, read before the store
        // takes it: the store reduces the event it is handed without a copy.
        let event = match &delivery {
            ClientDelivery::Live { message, .. } => match message.as_ref() {
                cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                    Some(event.as_ref())
                }
                _ => None,
            },
            ClientDelivery::ReplayEvent { event, .. } => Some(event.as_ref()),
            _ => None,
        };
        let live = matches!(&delivery, ClientDelivery::Live { .. });
        let run_ended = event.and_then(|event| {
            (live && matches!(&event.payload, EventPayload::RunFinished { .. }))
                .then_some(event.session_id)
        });
        let cost_changed = event.and_then(|event| {
            matches!(
                event.payload,
                EventPayload::ModelUsageRecorded { .. }
                    | EventPayload::InternalAgentUsageRecorded { .. }
            )
            .then_some(event.session_id)
        });
        let replay_ended = match &delivery {
            ClientDelivery::ReplayEnd { session_id, .. } => Some(*session_id),
            _ => None,
        };
        // A delegation's lifecycle is how a child's status reaches the
        // tree; a link, a run end, or a finished replay can change it too.
        let tree_changed = replay_ended.is_some_and(|session_id| self.in_viewed_tree(session_id))
            || event.is_some_and(|event| {
                matches!(
                    &event.payload,
                    EventPayload::ToolCallLinked { .. }
                        | EventPayload::RunFinished { .. }
                        | EventPayload::DelegationProgressed { .. }
                        | EventPayload::DelegationFinished { .. }
                        | EventPayload::DelegateChildTerminated { .. }
                ) && self.in_viewed_tree(event.session_id)
            });
        let outcome = self.update_store(|store| store.apply_delivery(delivery));
        if foreign_pick {
            self.draft_publish
                .update_value(DraftPublish::yield_to_foreign_pick);
            self.sync_draft();
            self.status
                .set("the model was picked by another client".into());
        }
        if let Some(session_id) = run_ended
            && !self.is_recent(session_id)
            && self.is_settled(session_id)
            && self
                .subscriptions
                .try_update_value(|subscriptions| subscriptions.release(session_id))
                .unwrap_or(false)
        {
            self.release(session_id);
        }
        if let Some(session_id) = replay_ended {
            let owned = self.owned(session_id);
            if let Some(subscribe) = self
                .subscriptions
                .try_update_value(|subscriptions| subscriptions.note_replay_end(session_id, owned))
                .flatten()
            {
                self.send_subscribe(subscribe);
            }
        }
        if tree_changed {
            self.refresh_tree();
        }
        // The background sheet closes with its last call, as in the TUI.
        if self.panel.get_untracked() == Some(Panel::Background)
            && untrack(|| self.background_calls()).is_empty()
        {
            self.panel.set(None);
        }
        match outcome {
            DeliveryOutcome::Applied => {
                self.sync_draft();
                self.restore_voided_inputs();
                if let Some(session_id) = cost_changed {
                    self.refresh_cost(session_id);
                }
            }
            DeliveryOutcome::Gap { cursor, .. } => {
                self.status
                    .set(format!("event gap after sequence {cursor}; replaying"));
            }
            DeliveryOutcome::ReplayFailed { session_id } => {
                self.status
                    .set("incomplete replay; retrying recovery".into());
                if let Some(client) = self.client() {
                    client.recover_session(session_id, true);
                }
                let owned = self.owned(session_id);
                self.subscriptions
                    .update_value(|subscriptions| subscriptions.recover(session_id, owned));
            }
        }
    }

    fn apply_sessions_change(self, change: SessionsChangedNotification) {
        match change {
            SessionsChangedNotification::Upserted { session } => {
                // A project's session count follows its sessions, which
                // `projects.changed` does not announce.
                let mut moved = false;
                self.sessions.update(|sessions| {
                    if let Some(existing) = sessions
                        .iter_mut()
                        .find(|existing| existing.session_id == session.session_id)
                    {
                        moved = existing.project != session.project;
                        *existing = *session;
                    } else if matches!(session.origin, SessionOrigin::Root) {
                        moved = session.project.is_some();
                        sessions.push(*session);
                    }
                });
                if moved {
                    self.refresh_projects();
                }
            }
            SessionsChangedNotification::Removed { session_id } => {
                self.forget_removed(session_id);
                self.refresh_projects();
            }
            SessionsChangedNotification::ApprovalsChanged {
                root_session_id, ..
            } => {
                if self.selected.get_untracked() == Some(root_session_id) {
                    self.refresh_approvals(root_session_id);
                }
            }
            SessionsChangedNotification::PermissionModeChanged {
                root_session_id,
                mode,
                ..
            } => self.permission_modes.update(|modes| {
                modes.insert(root_session_id, mode);
            }),
            SessionsChangedNotification::Resync => {
                let selected = self.selected.get_untracked();
                spawn_local(async move {
                    self.refresh_lists().await;
                    if let Some(session_id) = selected {
                        self.refresh_approvals(session_id);
                        self.load_permission_mode(session_id);
                    }
                });
            }
        }
    }

    /// A session the daemon removed (deleted with its tree): out of the
    /// list and the store; a page viewing it starts a new session instead.
    fn forget_removed(self, session_id: SessionId) {
        self.sessions
            .update(|sessions| sessions.retain(|session| session.session_id != session_id));
        let viewed = self.selected.get_untracked() == Some(session_id)
            || self.tree_root.get_untracked() == Some(session_id);
        if viewed && !self.new_session.get_untracked() {
            self.start_new_session();
        }
        // The daemon dropped its subscription already: nothing to send.
        self.subscriptions.update_value(|subscriptions| {
            subscriptions.release(session_id);
        });
        self.update_store(|store| store.forget_session(session_id));
        if self.delete_error.with_untracked(|error| {
            error
                .as_ref()
                .is_some_and(|(failed, _)| *failed == session_id)
        }) {
            self.delete_error.set(None);
        }
    }

    /// Deletes the root session `session_id` and its subagents, as the
    /// list showed it when the user confirmed. The list loses it when the
    /// daemon says `sessions.changed` `removed`; a refusal shows the
    /// daemon's message.
    pub fn delete_session(self, session_id: SessionId) {
        use cookie_agent_client_state::session_delete::{
            DAEMON_TOO_OLD_TO_DELETE, SessionDeleteOutcome, delete_session,
        };
        let Some(session) = self.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .cloned()
        }) else {
            return;
        };
        let Some(client) = self.client() else {
            return;
        };
        self.delete_error.set(None);
        spawn_local(async move {
            let message = match delete_session(&client, &session).await {
                SessionDeleteOutcome::Deleted(deleted) => {
                    let subagents = deleted.len().saturating_sub(1);
                    self.status.set(match subagents {
                        0 => "deleted the session".to_owned(),
                        1 => "deleted the session and its subagent".to_owned(),
                        count => format!("deleted the session and its {count} subagents"),
                    });
                    return;
                }
                SessionDeleteOutcome::DaemonTooOld => DAEMON_TOO_OLD_TO_DELETE.to_owned(),
                SessionDeleteOutcome::Failed(error) => crate::errors::describe(&error),
            };
            self.delete_error.set(Some((session_id, message)));
        });
    }

    fn resync_runtime(self) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            if let Ok(result) = client.runtime_snapshot().await {
                self.runtime.update_value(|runtime| {
                    runtime.install_current(result.snapshot);
                });
                self.runtime_revision.update(|revision| *revision += 1);
            }
        });
    }

    /// The root agents of the preset in use, as `/agent` offers them.
    pub fn selectable_agents(self) -> Vec<AgentDescriptor> {
        let preset = self.preset.get();
        self.with_runtime(|runtime| {
            runtime
                .snapshot()
                .map(|snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .filter(|agent| {
                            agent.runnable_as_root
                                && agent.mode != AgentMode::Internal
                                && agent.preset == preset
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Every preset the agents name.
    pub fn presets(self) -> Vec<String> {
        self.with_runtime(|runtime| {
            let mut presets = runtime
                .snapshot()
                .map(|snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .filter_map(|agent| agent.preset.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            presets.sort();
            presets.dedup();
            presets
        })
    }

    /// Every model the daemon offers.
    pub fn models(self) -> Vec<AvailableModelDescriptor> {
        self.with_runtime(|runtime| {
            runtime
                .snapshot()
                .map(|snapshot| snapshot.models.clone())
                .unwrap_or_default()
        })
    }

    /// The draft a fresh session starts from: the `primary` agent (else the
    /// first), its first live model (else the first model), as the TUI picks.
    fn default_draft(self) -> Option<RunSelection> {
        let agents = self.selectable_agents();
        let agent = agents
            .iter()
            .find(|agent| agent.id.as_str() == "primary")
            .or_else(|| agents.first())?;
        let models = self.models();
        let live = |selection: &ModelSelection| {
            models.iter().any(|model| {
                model.key == selection.model
                    && selection.variant.as_ref().is_none_or(|variant| {
                        model
                            .variants
                            .iter()
                            .any(|candidate| candidate.id == *variant)
                    })
            })
        };
        let model = cookie_agent_client_state::acp::acp_model(agent)
            .or_else(|| {
                agent
                    .resolved_fallback
                    .iter()
                    .find(|selection| live(selection))
                    .cloned()
            })
            .or_else(|| {
                models.first().map(|model| ModelSelection {
                    model: model.key.clone(),
                    variant: model.default_variant.clone(),
                })
            })?;
        Some(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: agent.preset.clone(),
        })
    }

    /// The draft follows the viewed session's selection while it is one
    /// this daemon can run.
    fn sync_draft(self) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        if self.new_session.get_untracked() {
            return;
        }
        let selection = self.store.with_value(|store| {
            store
                .sessions
                .get(&session_id)
                .and_then(|state| state.model_selection.selection.clone())
        });
        if let Some(selection) = selection
            && self.draft.get_untracked().as_ref() != Some(&selection)
        {
            self.draft.set(Some(selection));
        }
    }

    /// Picks `agent` (and its preferred model) for the draft. An ACP agent
    /// runs its synthetic model; one that is unavailable is refused.
    pub fn pick_agent(self, agent: &AgentDescriptor) {
        let attached = untrack(|| self.agent_runtimes());
        if !cookie_agent_client_state::acp::pickable(agent, &attached) {
            self.status
                .set(cookie_agent_client_state::acp::unpickable_reason(
                    agent, &attached,
                ));
            return;
        }
        let model = self
            .draft
            .get_untracked()
            .map(|draft| draft.model)
            .filter(|model| !cookie_agent_client_state::acp::is_acp_model(model))
            .or_else(|| self.default_draft().map(|draft| draft.model))
            .filter(|model| !cookie_agent_client_state::acp::is_acp_model(model));
        let Some(model) = cookie_agent_client_state::acp::acp_model(agent)
            .or_else(|| agent.resolved_fallback.first().cloned())
            .or(model)
        else {
            return;
        };
        self.set_draft(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: agent.preset.clone(),
        });
    }

    /// Picks `model` (and a variant of it) for the draft.
    pub fn pick_model(self, model: ModelSelection) {
        let Some(mut draft) = self.draft.get_untracked().or_else(|| self.default_draft()) else {
            return;
        };
        draft.model = model;
        self.set_draft(draft);
    }

    /// Picks the preset new sessions' agents come from.
    pub fn pick_preset(self, preset: Option<String>) {
        self.preset.set(preset);
        self.draft.set(self.default_draft());
    }

    fn set_draft(self, selection: RunSelection) {
        self.draft.set(Some(selection));
        if self.new_session.get_untracked() {
            return;
        }
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        // Shared once the pick settles; a later pick or another client's
        // supersedes it.
        let generation = self
            .draft_publish
            .try_update_value(DraftPublish::schedule)
            .unwrap_or_default();
        set_timeout(
            move || {
                if !self
                    .draft_publish
                    .with_value(|publish| publish.is_current(generation))
                    || self.selected.get_untracked() != Some(session_id)
                {
                    return;
                }
                let (Some(client), Some(selection)) = (self.client(), self.draft.get_untracked())
                else {
                    return;
                };
                spawn_local(async move {
                    if let Err(error) = client
                        .select_model(SessionSelectModelParams {
                            session_id,
                            selection,
                        })
                        .await
                    {
                        self.status.set(format!(
                            "model selection not shared with other clients: {}",
                            crate::errors::describe(&error)
                        ));
                    }
                });
            },
            DRAFT_PUBLISH_DELAY,
        );
    }

    /// Starts composing a new session: its runtime cards with only
    /// `global` selected, and the composer under them, whose first message
    /// creates the session. A project's defaults prefill the cards.
    /// Nothing is remembered from an earlier draft.
    pub fn start_new_session(self) {
        self.selection.set(crate::cards::Selection::global_only());
        self.new_project.set(None);
        self.card_errors.set(HashMap::new());
        self.creating.set(false);
        self.draft_error.set(None);
        self.poll_runtimes();
        self.new_session.set(true);
        self.selected.set(None);
        self.tree_root.set(None);
        self.tree.set(None);
        self.attachments.set(Vec::new());
        self.drop_draft_files();
        self.downscale_offer.set(None);
        self.draft_rules.set(Vec::new());
        self.composer.set(String::new());
        self.draft.set(self.default_draft());
        self.panel.set(None);
        self.sidebar_open.set(false);
        self.view.set(crate::layout::View::Chat);
    }

    /// Opens `session_id`: classifies who owns it, subscribes it, and reads
    /// its approvals and permission mode.
    pub fn open_session(self, session_id: SessionId) {
        if self.selected.get_untracked() != Some(session_id) {
            self.attachments.set(Vec::new());
            self.drop_draft_files();
            self.downscale_offer.set(None);
        }
        self.new_session.set(false);
        self.selected.set(Some(session_id));
        self.panel.set(None);
        self.sidebar_open.set(false);
        self.view.set(crate::layout::View::Chat);
        self.usage.set(None);
        self.user_menu.set(None);
        let generation = self.begin_ownership_classification(session_id);
        let connection = self.connection.get_value();
        self.locate_tree(session_id);
        self.note_viewed(session_id);
        let owned = self.owned(session_id);
        if let Some(subscribe) = self
            .subscriptions
            .try_update_value(|subscriptions| {
                subscriptions.subscribe(session_id, owned, SubscribeIntent::Open)
            })
            .flatten()
        {
            self.send_subscribe(subscribe);
        }
        self.sync_draft();
        self.refresh_approvals(session_id);
        self.load_permission_mode(session_id);
        self.refresh_cost(session_id);
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let outcome = classify_session(&client, session_id).await;
            if self.is_current(connection) {
                self.apply_ownership_classification(session_id, generation, outcome);
            }
        });
    }

    /// Records `session_id`'s tree as the most recently viewed: its
    /// sessions stay subscribed, and the tree this pushes out has its
    /// settled sessions released.
    fn note_viewed(self, session_id: SessionId) {
        let root = self.tree_root.get_untracked().unwrap_or(session_id);
        let tree = self.tree.get_untracked();
        let root_of = |session: SessionId| match &tree {
            Some(tree) if crate::tree::contains(tree, session) => tree.session.session_id,
            _ => session,
        };
        let released = self
            .subscriptions
            .try_update_value(|subscriptions| {
                subscriptions.note_viewed_tree(root, root_of, |session| self.is_settled(session))
            })
            .unwrap_or_default();
        for session in released {
            self.release(session);
        }
    }

    fn begin_ownership_classification(self, session_id: SessionId) -> u64 {
        self.next_ownership_classification
            .update_value(|next| *next = next.wrapping_add(1));
        let generation = self.next_ownership_classification.get_value();
        self.ownership_classifications
            .update_value(|classifications| {
                classifications.insert(session_id, generation);
            });
        if !self.owned(session_id) {
            self.read_only.update(|read_only| {
                read_only.insert(session_id);
            });
        }
        generation
    }

    fn apply_ownership_classification(
        self,
        session_id: SessionId,
        generation: u64,
        outcome: SessionOwnershipOutcome,
    ) {
        let current = self
            .ownership_classifications
            .with_value(|classifications| classifications.get(&session_id) == Some(&generation));
        if !current {
            return;
        }
        self.ownership_classifications
            .update_value(|classifications| {
                classifications.remove(&session_id);
            });
        let owned = match outcome {
            SessionOwnershipOutcome::Owned(session) => {
                self.sessions.update(|sessions| {
                    if let Some(existing) = sessions
                        .iter_mut()
                        .find(|existing| existing.session_id == session_id)
                    {
                        *existing = *session;
                    }
                });
                true
            }
            SessionOwnershipOutcome::Foreign => {
                if self.selected.get_untracked() == Some(session_id) {
                    self.status.set(
                        "This session is owned by another cookie process; read-only snapshot."
                            .into(),
                    );
                }
                false
            }
            SessionOwnershipOutcome::Failed(error) => {
                if self.selected.get_untracked() == Some(session_id) {
                    self.status.set(error);
                }
                false
            }
        };
        self.owned.update_value(|owned_sessions| {
            if owned {
                owned_sessions.insert(session_id);
            } else {
                owned_sessions.remove(&session_id);
            }
        });
        self.read_only.update(|read_only| {
            if owned {
                read_only.remove(&session_id);
            } else {
                read_only.insert(session_id);
            }
        });
        // Owned now: subscribe, or upgrade a snapshot subscription made
        // before ownership to a live one.
        if let Some(subscribe) = self
            .subscriptions
            .try_update_value(|subscriptions| {
                subscriptions.subscribe(session_id, owned, SubscribeIntent::Follow)
            })
            .flatten()
        {
            self.send_subscribe(subscribe);
        }
    }

    fn send_subscribe(self, subscribe: Subscribe) {
        let Some(client) = self.client() else {
            return;
        };
        let Subscribe {
            session_id,
            attempt,
            from_scratch,
        } = subscribe;
        let cursor = if from_scratch {
            None
        } else {
            self.store
                .with_value(|store| store.sessions.get(&session_id).map(|state| state.last_seq))
        };
        let connection = self.connection.get_value();
        spawn_local(async move {
            let outcome = match client.subscribe_events(session_id, cursor).await {
                Ok(()) => {
                    // Background calls are not in the event log: read them
                    // once subscribed; `background.changed` follows.
                    if let Ok(listed) = client.list_background(session_id).await {
                        self.update_store(|store| {
                            store
                                .sessions
                                .entry(session_id)
                                .or_default()
                                .background_calls = listed.calls;
                        });
                    }
                    SubscriptionOutcome::Established
                }
                Err(ClientError::ReplayInProgress) => SubscriptionOutcome::ReplayInProgress,
                Err(error) => SubscriptionOutcome::Failed(crate::errors::describe(&error)),
            };
            if !self.is_current(connection) {
                return;
            }
            if let SubscriptionOutcome::Failed(error) = &outcome
                && self.selected.get_untracked() == Some(session_id)
            {
                self.status.set(error.clone());
            }
            let owned = self.owned(session_id);
            if let Some(next) = self
                .subscriptions
                .try_update_value(|subscriptions| {
                    subscriptions.finish(session_id, attempt, &outcome, owned)
                })
                .flatten()
            {
                self.send_subscribe(next);
            }
        });
    }

    /// Ends the live tail of a session whose subscription was released,
    /// and drops its projection: nothing shows it, and opening it again
    /// replays it from the start.
    fn release(self, session_id: SessionId) {
        self.store.update_value(|store| {
            store.evict_session(session_id);
        });
        if let Some(client) = self.client() {
            spawn_local(async move {
                let _ = client.unsubscribe_events(session_id).await;
            });
        }
    }

    fn schedule_subscription_retry(self, session_id: Option<SessionId>) {
        let Some(selected) = self.selected.get_untracked() else {
            return;
        };
        if session_id.is_some_and(|session_id| session_id != selected) {
            return;
        }
        set_timeout(
            move || {
                if self.selected.get_untracked() != Some(selected)
                    || self
                        .subscriptions
                        .with_value(|subscriptions| subscriptions.contains(selected))
                {
                    return;
                }
                let owned = self.owned(selected);
                let subscribe = self.subscriptions.try_update_value(|subscriptions| {
                    subscriptions.subscribe_from_scratch(selected, owned)
                });
                if let Some(subscribe) = subscribe {
                    self.send_subscribe(subscribe);
                }
            },
            SUBSCRIPTION_RETRY_DELAY,
        );
    }

    fn is_recent(self, session_id: SessionId) -> bool {
        self.subscriptions
            .with_value(|subscriptions| subscriptions.is_recent_tree(session_id))
    }

    fn is_settled(self, session_id: SessionId) -> bool {
        self.store.with_value(|store| {
            store
                .sessions
                .get(&session_id)
                .is_none_or(cookie_agent_client_state::SessionState::is_settled)
        })
    }

    /// Whether the composer may submit to the viewed session.
    pub fn writable(self) -> bool {
        self.new_session.get()
            || self.selected.get().is_some_and(|session_id| {
                !self
                    .read_only
                    .with(|read_only| read_only.contains(&session_id))
            })
    }

    /// Submits the composer's text: to the viewed session (steering or
    /// queueing behind a running run), or creating the new session first.
    /// Text that starts with `/` is a message too: commands are in the
    /// session's ⋮ menu.
    pub fn submit(self, delivery: InputDelivery) {
        let text = self.composer.get_untracked();
        let files = self.attachments.get_untracked();
        if text.trim().is_empty() && files.is_empty() {
            return;
        }
        if !self.writable() {
            self.status
                .set("This session is owned by another cookie process; input is disabled.".into());
            return;
        }
        if self.new_session.get_untracked() && !self.may_create() {
            return;
        }
        let Some(content) = (ComposerInput {
            text: text.clone(),
            files: files.clone(),
        })
        .content() else {
            return;
        };
        self.composer.set(String::new());
        self.attachments.set(Vec::new());
        self.send_content(content, delivery, text, files);
    }

    /// Sends `content` to the viewed session, or creates the new session
    /// with it. On failure `text` and `files` go back to an empty composer.
    pub fn send_content(
        self,
        content: cookie_agent_protocol::UserContent,
        delivery: InputDelivery,
        text: String,
        files: Vec<cookie_agent_protocol::ArtifactRef>,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        let new_session = self.new_session.get_untracked();
        if new_session && !self.may_create() {
            self.bounce_draft(text, files);
            return;
        }
        self.sent.notify();
        let selection = self.draft.get_untracked();
        spawn_local(async move {
            let mut content = content;
            let mut files = files;
            let session_id = if new_session {
                self.creating.set(true);
                self.draft_error.set(None);
                self.card_errors.set(HashMap::new());
                let created = self.create_session(&client, selection.clone()).await;
                self.creating.set(false);
                let session_id = match created {
                    Ok(session_id) => session_id,
                    Err(refusal) => {
                        self.refuse_draft(refusal, text, files);
                        return;
                    }
                };
                // The draft's files go into the session now, and the
                // message names them by their own references.
                if self.holds_draft_files(&files) {
                    match self.upload_draft_files(&client, session_id, files).await {
                        Ok(uploaded) => {
                            files = uploaded;
                            let rebuilt = ComposerInput {
                                text: text.clone(),
                                files: files.clone(),
                            }
                            .content();
                            if let Some(rebuilt) = rebuilt {
                                content = rebuilt;
                            }
                        }
                        Err(uploaded) => {
                            // As a failed submit: the message goes back to
                            // the session's composer.
                            self.bounce_draft(text, uploaded);
                            return;
                        }
                    }
                }
                session_id
            } else {
                match self.selected.get_untracked() {
                    Some(session_id) => session_id,
                    None => return,
                }
            };
            let result = client
                .submit_input(SessionSubmitInputParams {
                    session_id,
                    content,
                    delivery,
                    selection,
                    client_run_id: client_run_id(),
                    reset_fallback: false,
                })
                .await;
            match result {
                Ok(SessionSubmitInputResult::Handled { reason }) => self.status.set(reason),
                Ok(SessionSubmitInputResult::Queued { .. }) => {
                    self.status.set("queued behind the running run".into());
                }
                Ok(_) => {}
                Err(error) => {
                    // The text goes back to the composer, as in the TUI.
                    self.composer.update(|composer| {
                        if composer.is_empty() {
                            *composer = text;
                        }
                    });
                    self.attachments.update(|attachments| {
                        if attachments.is_empty() {
                            *attachments = files;
                        }
                    });
                    self.status.set(crate::errors::describe(&error));
                }
            }
        });
    }

    /// Whether the first message may create the new session now: not while
    /// it is being created, nor while a selected card cannot be used, which
    /// the composer's notice then says.
    fn may_create(self) -> bool {
        if self.creating.get_untracked() {
            self.status.set("the new session is being created…".into());
            return false;
        }
        let cards = untrack(|| self.cards());
        match self
            .selection
            .with_untracked(|selection| selection.blocker(&cards))
        {
            Some(blocker) => {
                self.status.set(blocker.clone());
                self.draft_error.set(Some(blocker));
                false
            }
            None => true,
        }
    }

    /// Puts a message the new session did not take back into the composer,
    /// ahead of anything typed meanwhile.
    fn bounce_draft(self, text: String, files: Vec<cookie_agent_protocol::ArtifactRef>) {
        self.composer.update(|composer| {
            *composer = match (text.is_empty(), composer.is_empty()) {
                (_, true) => text,
                (true, false) => std::mem::take(composer),
                (false, false) => format!("{text}\n\n{composer}"),
            };
        });
        self.attachments.update(|attachments| {
            let typed = std::mem::take(attachments);
            *attachments = files;
            attachments.extend(typed);
        });
    }

    /// The daemon did not create the new session: the draft stays as it
    /// was, the message goes back into the composer, and the error shows on
    /// the card of the runtime it names, else beside the composer.
    fn refuse_draft(
        self,
        refusal: DraftRefusal,
        text: String,
        files: Vec<cookie_agent_protocol::ArtifactRef>,
    ) {
        // The page left the draft meanwhile: the message has nowhere to go
        // back to but the status line.
        if !self.new_session.get_untracked() {
            self.status.set(format!(
                "the new session was not created: {}",
                refusal.message
            ));
            return;
        }
        self.bounce_draft(text, files);
        let card = crate::cards::failing_request(&refusal.message, &refusal.requests);
        match card {
            Some((key, index)) => {
                let label = untrack(|| self.cards())
                    .into_iter()
                    .find(|card| card.key() == key)
                    .map_or_else(|| key.clone(), |card| card.label());
                let notice =
                    format!("No session was created: {label} did not start; see its card.");
                self.status.set(notice.clone());
                self.draft_error.set(Some(notice));
                self.card_errors
                    .set(HashMap::from([((key.clone(), index), refusal.line)]));
                crate::browser::reveal(&format!("#new-session [data-card=\"{key}\"]"));
            }
            None => {
                self.status.set(format!(
                    "the new session was not created: {}",
                    refusal.message
                ));
                self.draft_error.set(Some(refusal.message));
            }
        }
    }

    /// What the new session is created with: whether it attaches `global`,
    /// and its runtimes, each with the card field it came from; `None` for
    /// a draft still on its project's defaults, which the daemon attaches.
    fn draft_requests(self) -> (bool, Option<Vec<crate::cards::KeyedRequest>>) {
        let selection = self.selection.get_untracked();
        if let Some(defaults) = untrack(|| self.draft_project_defaults())
            && selection
                == crate::cards::Selection::of_project(&defaults, &untrack(|| self.homes()))
        {
            return (true, None);
        }
        let (global, requests) = selection.keyed_requests(&untrack(|| self.cards()));
        (global, Some(requests))
    }

    async fn create_session(
        self,
        client: &Client,
        selection: Option<RunSelection>,
    ) -> Result<SessionId, DraftRefusal> {
        let refused = |message: String| DraftRefusal {
            line: message.clone(),
            message,
            requests: Vec::new(),
        };
        let selection = selection
            .or_else(|| self.default_draft())
            .ok_or_else(|| refused("no agent and model to start a session with".into()))?;
        // The selected cards, in card order, at the workspaces as typed: the
        // daemon starts every runtime that is not running and resolves each
        // workspace, or creates nothing. A draft still on its project's
        // defaults leaves them to the daemon.
        let project = self
            .new_project
            .get_untracked()
            .filter(|_| self.has_projects.get_untracked());
        let (global, keyed) = self.draft_requests();
        let runtimes = keyed.as_ref().map(|keyed| {
            keyed
                .iter()
                .map(|(_, _, request)| request.clone())
                .collect::<Vec<_>>()
        });
        let created = client
            .create_session(SessionCreateParams {
                permission_mode: self.new_session_mode.get_untracked(),
                permission_rules: self.draft_rules.get_untracked(),
                runtimes,
                global,
                project,
                ..SessionCreateParams::new(selection)
            })
            .await
            .map_err(|error| DraftRefusal {
                line: crate::errors::describe(&error),
                message: crate::errors::describe_full(&error),
                requests: keyed.unwrap_or_default(),
            })?;
        // This browser suggests the workspaces it opened next time.
        for attachment in &created.runtimes {
            if let Some(workspace) = &attachment.workspace {
                let key = crate::cards::recent_key(attachment.runtime_id.as_str());
                let recent = crate::cards::recent_from_storage(browser::load(&key).as_deref());
                if let Ok(stored) =
                    serde_json::to_string(&crate::cards::remember(&recent, workspace))
                {
                    browser::store(&key, &stored);
                }
            }
        }
        let session_id = created.session.session_id;
        if created.session.project.is_some() {
            self.refresh_projects();
        }
        self.owned.update_value(|owned| {
            owned.insert(session_id);
        });
        self.sessions.update(|sessions| {
            if !sessions
                .iter()
                .any(|session| session.session_id == session_id)
            {
                sessions.push(created.session.clone());
            }
        });
        if let Some(mode) = self.new_session_mode.get_untracked() {
            self.permission_modes.update(|modes| {
                modes.insert(session_id, mode);
            });
        }
        self.new_session_mode.set(None);
        self.draft_rules.set(Vec::new());
        self.draft_error.set(None);
        self.card_errors.set(HashMap::new());
        self.new_session.set(false);
        self.selected.set(Some(session_id));
        self.reroot_tree(session_id);
        self.note_viewed(session_id);
        let subscribe = self
            .subscriptions
            .try_update_value(|subscriptions| {
                subscriptions.subscribe(session_id, true, SubscribeIntent::Follow)
            })
            .flatten();
        if let Some(subscribe) = subscribe {
            self.send_subscribe(subscribe);
        }
        Ok(session_id)
    }

    /// Unsent input a run end left without a destination comes back into
    /// the composer of the client that wrote it.
    fn restore_voided_inputs(self) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        let own = self.client().and_then(|client| client.origin());
        let inputs = self
            .store
            .try_update_value(|store| store.take_voided_inputs(session_id, own.as_ref()))
            .unwrap_or_default();
        if inputs.is_empty() {
            return;
        }
        let text = inputs
            .iter()
            .map(|input| input.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.composer.update(|composer| {
            if composer.is_empty() {
                *composer = text;
            } else {
                composer.push('\n');
                composer.push_str(&text);
            }
        });
        self.status
            .set("unsent message restored to the composer".into());
    }

    /// This client's pending and queued inputs of the viewed session, as
    /// `(seq, text, own)`, in order.
    pub fn queue(self) -> Vec<(u64, String, bool)> {
        let Some(session_id) = self.selected.get() else {
            return Vec::new();
        };
        let own = self.client().and_then(|client| client.origin());
        self.with_store(|store| {
            let Some(state) = store.sessions.get(&session_id) else {
                return Vec::new();
            };
            state
                .pending_inputs
                .iter()
                .map(|input| {
                    (
                        input.admission_seq,
                        format!("steering: {}", input.text),
                        input.origin.as_ref() == own.as_ref(),
                    )
                })
                .chain(state.queued_inputs.entries().iter().map(|input| {
                    (
                        input.queued_seq,
                        format!(
                            "queued: {}",
                            cookie_agent_client_state::user_content_text(&input.content)
                        ),
                        input.origin.as_ref() == own.as_ref(),
                    )
                }))
                .collect()
        })
    }

    /// Takes back this client's newest pending or queued input.
    pub fn recall_newest(self) {
        match self
            .queue()
            .into_iter()
            .filter(|(_, _, own)| *own)
            .map(|(seq, _, _)| seq)
            .max()
        {
            Some(seq) => self.recall(seq),
            None => self
                .status
                .set("no pending message of yours to recall".into()),
        }
    }

    /// Takes back this client's pending or queued input `seq` into the
    /// composer (`session.recall_input`).
    pub fn recall(self, seq: u64) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            match client
                .recall_input(SessionRecallInputParams { session_id, seq })
                .await
            {
                Ok(SessionRecallInputResult::Recalled { content }) => {
                    let text = ComposerInput::of(&content).text;
                    self.composer.update(|composer| {
                        if composer.is_empty() {
                            *composer = text;
                        } else {
                            composer.push('\n');
                            composer.push_str(&text);
                        }
                    });
                }
                Ok(SessionRecallInputResult::Gone) => {
                    self.status.set("that message is no longer pending".into());
                }
                Ok(SessionRecallInputResult::NotOwner) => self
                    .status
                    .set("only the client that sent a message can recall it".into()),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// The viewed session's running run.
    pub fn active_run(self) -> Option<cookie_agent_protocol::RunId> {
        let session_id = self.selected.get()?;
        self.with_store(|store| store.sessions.get(&session_id)?.active_run)
    }

    /// Opens or closes `key` (a tool call, or a row's block key) in the
    /// viewed session's transcript.
    pub fn toggle_expanded(self, key: String) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        self.expanded.update(|expanded| {
            let opened = expanded.entry(session_id).or_default();
            if !opened.remove(&key) {
                opened.insert(key);
            }
        });
    }

    /// Whether the viewed session has a run or a compaction to stop, as
    /// the composer's Stop and Escape do.
    pub fn stoppable(self) -> bool {
        let Some(session_id) = self.selected.get() else {
            return false;
        };
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .is_some_and(|state| state.active_run.is_some() || state.is_compacting())
        })
    }

    /// Whether the viewed session runs an ACP agent that cannot take a
    /// message into its running turn: a steered message waits for its next
    /// turn, so the Steer button reads as queueing.
    pub fn unsteerable(self) -> bool {
        let Some(session_id) = self.selected.get() else {
            return false;
        };
        self.with_store(|store| {
            store.sessions.get(&session_id).is_some_and(|state| {
                state.acp.steers_into_turn(state.run_agent.as_ref()) == Some(false)
            })
        })
    }

    /// Cancels the viewed session's running run (`run.cancel`).
    pub fn cancel(self) {
        let (Some(client), Some(run_id)) = (self.client(), self.active_run()) else {
            if self.compacting() {
                self.cancel_compaction();
            } else {
                self.status.set("nothing is running".into());
            }
            return;
        };
        spawn_local(async move {
            match client.cancel_run(RunCancelParams { run_id }).await {
                Ok(_) => self.status.set("cancelling…".into()),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Reads the escalated approvals of `root_session_id`'s tree.
    pub fn refresh_approvals(self, root_session_id: SessionId) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client
                .list_approvals(ApprovalListParams {
                    root_session_id,
                    status: Some(ApprovalStatus::Escalated),
                })
                .await
            {
                Ok(result) => {
                    self.update_store(|store| {
                        store.apply_approval_list(&[root_session_id], result)
                    });
                }
                Err(error) => self.status.set(format!(
                    "approval list refresh failed: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }

    /// The approvals waiting for an answer in the viewed session.
    pub fn approvals(self) -> Vec<cookie_agent_client_state::ApprovalState> {
        let Some(session_id) = self.selected.get() else {
            return Vec::new();
        };
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .map(|state| {
                    state
                        .approvals
                        .iter()
                        .filter(|approval| approval.is_visible_user_escalation())
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Answers an approval at the revision and fingerprint it was shown at.
    pub fn respond(
        self,
        approval: cookie_agent_client_state::ApprovalState,
        decision: ApprovalUserDecision,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .respond_approval(ApprovalRespondParams {
                    session_id: approval.session_id,
                    approval_id: approval.approval_id,
                    request_revision: approval.request_revision,
                    operation_fingerprint: approval.operation_fingerprint,
                    client_response_id: ClientResponseId::new(uuid::Uuid::now_v7().to_string())
                        .expect("a uuid is a client response id"),
                    decision,
                    feedback: None,
                })
                .await;
            match result {
                Ok(_) => self.status.set("approval answered".into()),
                Err(error) => self.status.set(format!(
                    "approval not answered: {}",
                    crate::errors::describe(&error)
                )),
            }
            if let Some(session_id) = self.selected.get_untracked() {
                self.refresh_approvals(session_id);
            }
        });
    }

    fn load_permission_mode(self, session_id: SessionId) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            if let Ok(result) = client
                .get_session_permissions(SessionPermissionGetParams { session_id })
                .await
                && let Some(mode) = result.current_mode
            {
                self.permission_modes.update(|modes| {
                    modes.insert(session_id, mode);
                });
            }
        });
    }

    /// The mode the status bar shows: the new session's while one is being
    /// composed, else the viewed session's.
    pub fn permission_mode(self) -> PermissionMode {
        if self.new_session.get() {
            return self.new_session_mode.get().unwrap_or_default();
        }
        self.selected
            .get()
            .and_then(|session_id| {
                self.permission_modes
                    .with(|modes| modes.get(&session_id).copied())
            })
            .unwrap_or_default()
    }

    /// Sets the permission mode: the new session's, or the viewed
    /// session tree's (`session.set_permission_mode`).
    pub fn set_permission_mode(self, mode: PermissionMode) {
        if self.new_session.get_untracked() {
            self.new_session_mode.set(Some(mode));
            return;
        }
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        let previous = self
            .permission_modes
            .with_untracked(|modes| modes.get(&session_id).copied());
        self.permission_modes.update(|modes| {
            modes.insert(session_id, mode);
        });
        spawn_local(async move {
            if let Err(error) = client
                .set_permission_mode(SessionSetPermissionModeParams { session_id, mode })
                .await
            {
                self.permission_modes.update(|modes| match previous {
                    Some(previous) => {
                        modes.insert(session_id, previous);
                    }
                    None => {
                        modes.remove(&session_id);
                    }
                });
                self.status.set(format!(
                    "permission mode update failed: {}",
                    crate::errors::describe(&error)
                ));
            }
        });
    }

    /// Reads the session's cost again once usage events settle.
    fn refresh_cost(self, session_id: SessionId) {
        self.cost_debounce.update_value(|debounce| {
            *debounce.entry(session_id).or_default() += 1;
        });
        let generation = self
            .cost_debounce
            .with_value(|debounce| debounce.get(&session_id).copied().unwrap_or_default());
        set_timeout(
            move || {
                let current = self
                    .cost_debounce
                    .with_value(|debounce| debounce.get(&session_id) == Some(&generation));
                let Some(client) = self.client().filter(|_| current) else {
                    return;
                };
                spawn_local(async move {
                    if let Ok(result) = client
                        .session_usage(SessionUsageParams { session_id })
                        .await
                        && let Some(cost) = result.usage.estimated_cost_usd
                    {
                        self.costs.update(|costs| {
                            costs.insert(session_id, cost);
                        });
                    }
                });
            },
            COST_DEBOUNCE,
        );
    }

    /// Reads the `/usage` panel's figures for the viewed session.
    pub fn load_usage(self) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            self.usage.set(None);
            return;
        };
        spawn_local(async move {
            let session = client
                .session_usage(SessionUsageParams { session_id })
                .await
                .map(|result| result.usage)
                .map_err(|error| crate::errors::describe_full(&error));
            let tree = client
                .session_tree_usage(SessionUsageParams { session_id })
                .await
                .map(|result| (result.usage, result.session_count))
                .map_err(|error| crate::errors::describe_full(&error));
            self.usage.set(Some(UsageView { session, tree }));
        });
    }

    /// Opens or puts away the complete content of a cut display block.
    pub fn toggle_full_content(self, call_id: ToolCallId, block: usize) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        let read = self.update_store(|store| {
            store
                .sessions
                .get_mut(&session_id)
                .and_then(|state| state.toggle_full_content(call_id, block))
        });
        let (Some(read), Some(client)) = (read, self.client()) else {
            return;
        };
        spawn_local(async move {
            let state = FullContentState::of_read(
                client
                    .read_artifact(ArtifactReadParams {
                        session_id,
                        sha256: read.artifact.sha256.clone(),
                        offset: 0,
                        length: read.length,
                    })
                    .await
                    .map_err(|error| crate::errors::describe_full(&error)),
            );
            self.update_store(|store| {
                if let Some(session) = store.sessions.get_mut(&session_id) {
                    session.finish_full_content(call_id, block, state);
                }
            });
        });
    }

    /// Sets the lowest diagnostic level the transcript shows, kept in the
    /// browser.
    pub fn set_level(self, level: EventLevel) {
        browser::store(EVENT_LEVEL_KEY, level.name());
        self.level.set(level);
    }

    /// Opens the sheet of a ⋮ menu entry.
    pub fn run_command(self, command: crate::commands::Command) {
        use crate::commands::Command;
        match command {
            Command::Rename => self.begin_title_edit(),
            Command::Compact => {
                if self.selected.get_untracked().is_none() || self.new_session.get_untracked() {
                    self.status
                        .set("select a session before compacting context".into());
                    return;
                }
                self.panel.set(Some(Panel::Compact));
            }
            Command::Goal => self.goal_command(crate::goal::GoalCommand::Show),
            Command::Schedule => self.schedule_prompt(),
            Command::Skills => self.open_skills(String::new()),
            Command::Background => self.open_background(),
            Command::Runtime => self.open_runtimes(),
            Command::Permissions => self.open_permissions(),
            Command::Usage => {
                self.load_usage();
                self.panel.set(Some(Panel::Usage));
            }
            Command::Events => self.panel.set(Some(Panel::Events)),
            Command::Preset => self.panel.set(Some(Panel::Presets)),
        }
    }

    /// A media block's bytes, read in chunks, with its media type.
    pub async fn read_media(self, sha256: String) -> Result<(Vec<u8>, String), String> {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return Err("not connected".into());
        };
        let mut bytes = Vec::new();
        loop {
            let read = client
                .read_artifact(ArtifactReadParams {
                    session_id,
                    sha256: sha256.clone(),
                    offset: bytes.len() as u64,
                    length: cookie_agent_protocol::ARTIFACT_CHUNK_BYTES,
                })
                .await
                .map_err(|error| crate::errors::describe_full(&error))?;
            use base64::Engine as _;
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(read.data)
                .map_err(|error| error.to_string())?;
            let done = chunk.is_empty() || bytes.len() + chunk.len() >= read.total_bytes as usize;
            bytes.extend(chunk);
            if done {
                return Ok((bytes, read.media_type));
            }
        }
    }
}

/// The level a stored `/events` choice names; the TUI's default without
/// one.
pub fn level_from_storage(stored: Option<&str>) -> EventLevel {
    match stored {
        Some("debug") => EventLevel::Debug,
        Some("info") => EventLevel::Info,
        Some("warning") => EventLevel::Warning,
        Some("error") => EventLevel::Error,
        _ => EventLevel::DEFAULT_MINIMUM,
    }
}

fn client_run_id() -> ClientRunId {
    ClientRunId::new(format!("web-{}", uuid::Uuid::now_v7())).expect("a bounded client run id")
}

#[cfg(test)]
mod tests {
    use cookie_agent_client_state::EventLevel;

    use super::level_from_storage;

    #[test]
    fn a_stored_level_wins_over_the_shared_default() {
        assert_eq!(level_from_storage(Some("info")), EventLevel::Info);
        assert_eq!(level_from_storage(Some("debug")), EventLevel::Debug);
        assert_eq!(level_from_storage(None), EventLevel::DEFAULT_MINIMUM);
        assert_eq!(
            level_from_storage(Some("loud")),
            EventLevel::DEFAULT_MINIMUM
        );
    }
}
