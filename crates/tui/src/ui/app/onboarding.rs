//! Onboarding (`docs/site/specs/default-models-and-onboarding.md` section
//! 9): the notice offered while the daemon is not set up, and the
//! `/onboard` panel with its four steps. Step 1 hands over to the provider
//! connect panel and steps 2 to 4 to the model picker; each returns to the
//! panel when it closes. Everything the panel does is a daemon call:
//! `onboarding.get`, `onboarding.run`, and `provider.connect`. There is no
//! starter kit: the daemon keeps the official agents itself.

use std::path::PathBuf;

use cookie_agent_client_state::onboarding::{
    self as rules, OnboardingStep, default_model_variant, finish_refusal, first_step, model_input,
    model_label, run_summary, small_model_variant, utility_model_variant,
};
use cookie_agent_client_state::refresh::{Latest, Ticket};
use cookie_agent_protocol::{
    AgentId, ModelKey, OnboardingRequirement, OnboardingRunParams, OnboardingRunResult,
    OnboardingStatus, VariantId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};

use super::{App, Modal, RpcUpdate, centered, paint_panel};

/// The file under the user data root where the TUI remembers its own
/// state between runs: for now, which notice "Not now" dismissed.
const STATE_FILE: &str = "tui-state.json";

/// Which model a picker hand-over chooses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum ModelSlot {
    Default,
    Small,
    Utility,
}

/// A button of the notice or of the panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum OnboardingAction {
    /// The notice's "Set up": opens the panel.
    SetUp,
    /// The notice's or the panel's "Not now": hides the notice until what
    /// the daemon misses changes.
    NotNow,
    /// Step 1: the provider connect panel.
    Connect,
    /// Steps 2 to 4: the model picker.
    ChooseModel,
    Back,
    Next,
    Finish,
    /// After a run: a new-session draft on the default agent.
    StartSession,
    Close,
}

impl OnboardingAction {
    const fn label(self) -> &'static str {
        match self {
            Self::SetUp => "Set up",
            Self::NotNow => "Not now",
            Self::Connect => "connect a provider",
            Self::ChooseModel => "choose",
            Self::Back => "back",
            Self::Next => "next",
            Self::Finish => "finish",
            Self::StartSession => "start a session",
            Self::Close => "close",
        }
    }

    const fn key_label(self) -> &'static str {
        match self {
            Self::SetUp | Self::Connect | Self::ChooseModel | Self::StartSession => "enter",
            Self::NotNow => "n",
            Self::Back => "←",
            Self::Next => "→",
            Self::Finish => "f",
            Self::Close => "esc",
        }
    }
}

/// The notice's and the panel's click targets.
#[derive(Default)]
pub(in crate::ui) struct OnboardingHits {
    pub(in crate::ui) notice: Vec<(Rect, OnboardingAction)>,
    pub(in crate::ui) tabs: Vec<(Rect, OnboardingStep)>,
    pub(in crate::ui) actions: Vec<(Rect, OnboardingAction)>,
}

/// What the TUI keeps between runs.
#[derive(Debug, Default, Deserialize, Serialize)]
struct StoredState {
    /// What the daemon missed when "Not now" was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    onboarding_not_now: Option<Vec<OnboardingRequirement>>,
}

#[derive(Default)]
pub(in crate::ui) struct OnboardingState {
    /// `onboarding.get` or the last `onboarding.changed`; `None` before the
    /// daemon answers, and from a daemon without onboarding.
    pub(in crate::ui) status: Option<OnboardingStatus>,
    /// The `onboarding.get` reads: an answer older than another read, or
    /// than an `onboarding.changed`, is not installed.
    status_reads: Latest,
    /// Where "Not now" is remembered; `None` keeps it in memory only.
    pub(in crate::ui) state_file: Option<PathBuf>,
    /// What the daemon missed when "Not now" was chosen.
    pub(in crate::ui) not_now: Option<Vec<OnboardingRequirement>>,
    pub(in crate::ui) step: Option<OnboardingStep>,
    pub(in crate::ui) default_choice: Option<(ModelKey, Option<VariantId>)>,
    pub(in crate::ui) small_choice: Option<(ModelKey, Option<VariantId>)>,
    pub(in crate::ui) utility_choice: Option<(ModelKey, Option<VariantId>)>,
    pub(in crate::ui) running: bool,
    pub(in crate::ui) summary: Option<Vec<String>>,
    pub(in crate::ui) error: Option<String>,
    /// The panel handed over to another modal and reopens when it closes.
    pub(in crate::ui) resume: bool,
    /// The model picker is choosing this model for the panel.
    pub(in crate::ui) picking: Option<ModelSlot>,
}

impl OnboardingState {
    /// Reads what "Not now" remembered from `path`.
    pub(in crate::ui) fn with_state_file(path: PathBuf) -> Self {
        let not_now = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StoredState>(&bytes).ok())
            .and_then(|state| state.onboarding_not_now);
        Self {
            state_file: Some(path),
            not_now,
            ..Self::default()
        }
    }

    /// The default state file, `~/.cookie-agent/tui-state.json`.
    pub(in crate::ui) fn default_state_file() -> Option<PathBuf> {
        cookie_agent_protocol::paths::user_data_root()
            .ok()
            .map(|root| root.join(STATE_FILE))
    }

    fn remember_not_now(&mut self, missing: Option<Vec<OnboardingRequirement>>) {
        self.not_now = missing;
        let Some(path) = &self.state_file else {
            return;
        };
        let state = StoredState {
            onboarding_not_now: self.not_now.clone(),
        };
        if let Ok(text) = serde_json::to_vec_pretty(&state) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, text);
        }
    }

    fn step(&self) -> OnboardingStep {
        self.step.unwrap_or(OnboardingStep::Provider)
    }

    fn choice(&self, slot: ModelSlot) -> Option<&(ModelKey, Option<VariantId>)> {
        match slot {
            ModelSlot::Default => self.default_choice.as_ref(),
            ModelSlot::Small => self.small_choice.as_ref(),
            ModelSlot::Utility => self.utility_choice.as_ref(),
        }
    }
}

impl App {
    /// Whether the notice line shows: the daemon is not set up, and "Not
    /// now" was not chosen for what it misses now.
    pub(in crate::ui) fn onboarding_notice_visible(&self) -> bool {
        self.onboarding.status.as_ref().is_some_and(|status| {
            !status.ready && self.onboarding.not_now.as_ref() != Some(&status.missing)
        })
    }

    /// Asks the daemon whether it is set up.
    pub(in crate::ui) fn poll_onboarding(&mut self) {
        let read = self.onboarding.status_reads.begin();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            // A daemon without onboarding leaves the status unknown.
            let status = client.get_onboarding().await.ok();
            let _ = updates.send(RpcUpdate::OnboardingStatus { read, status });
        });
    }

    /// `onboarding.get`'s answer to `read`, unless a later read or an
    /// `onboarding.changed` superseded it: the daemon answers requests
    /// concurrently, and the answer may have been read before the change.
    pub(in crate::ui) fn install_onboarding_read(
        &mut self,
        read: Ticket,
        status: Option<OnboardingStatus>,
    ) {
        if self.onboarding.status_reads.is_current(read) {
            self.install_onboarding_status(status);
        }
    }

    /// The daemon's answer, from `onboarding.get` or `onboarding.changed`.
    pub(in crate::ui) fn install_onboarding_status(&mut self, status: Option<OnboardingStatus>) {
        if let Some(status) = status {
            self.onboarding.status = Some(status);
        }
    }

    /// `onboarding.changed`: newer than any `onboarding.get` on its way.
    pub(in crate::ui) fn note_onboarding_changed(&mut self, status: OnboardingStatus) {
        self.onboarding.status_reads.supersede();
        self.install_onboarding_status(Some(status));
    }

    pub(super) fn open_onboarding_panel(&mut self) {
        let status = self.onboarding.status.clone();
        let status_reads = self.onboarding.status_reads;
        let state_file = self.onboarding.state_file.take();
        let not_now = self.onboarding.not_now.take();
        self.onboarding = OnboardingState {
            status,
            status_reads,
            state_file,
            not_now,
            ..OnboardingState::default()
        };
        self.onboarding.step = Some(
            self.onboarding
                .status
                .as_ref()
                .map_or(OnboardingStep::Provider, first_step),
        );
        self.modal = Modal::Onboarding;
        self.poll_onboarding();
    }

    /// Reopens the panel after the modal it handed over to closed.
    pub(in crate::ui) fn resume_onboarding(&mut self) {
        if self.onboarding.resume && self.modal == Modal::None {
            self.onboarding.resume = false;
            self.onboarding.picking = None;
            self.modal = Modal::Onboarding;
            self.poll_onboarding();
        }
    }

    /// A tab's click: that step.
    pub(in crate::ui) fn show_onboarding_step(&mut self, step: OnboardingStep) {
        if self.onboarding.summary.is_none() {
            self.go_to_step(step);
        }
    }

    fn go_to_step(&mut self, step: OnboardingStep) {
        self.onboarding.step = Some(step);
    }

    /// Why Finish is disabled, if it is.
    pub(in crate::ui) fn onboarding_finish_refusal(&self) -> Option<String> {
        let Some(status) = &self.onboarding.status else {
            return Some("Waiting for the daemon.".into());
        };
        if self.onboarding.running {
            return Some("Setting up…".into());
        }
        finish_refusal(status, self.onboarding.default_choice.is_some())
    }

    fn finish_onboarding(&mut self) {
        if let Some(reason) = self.onboarding_finish_refusal() {
            self.status = reason;
            return;
        }
        let input = |choice: Option<&(ModelKey, Option<VariantId>)>| {
            choice.and_then(|(model, variant)| {
                self.model_descriptor(model)
                    .map(|descriptor| model_input(descriptor, variant.as_ref()))
            })
        };
        let params = OnboardingRunParams {
            install_kit: false,
            source: None,
            default_model: input(self.onboarding.choice(ModelSlot::Default)),
            small_model: input(self.onboarding.choice(ModelSlot::Small)),
            utility_model: input(self.onboarding.choice(ModelSlot::Utility)),
        };
        self.onboarding.running = true;
        self.onboarding.error = None;
        // The daemon fetches the official agents first when it has none.
        self.status = "setting up…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .run_onboarding(params)
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::OnboardingRun { result });
        });
    }

    pub(in crate::ui) fn install_onboarding_run(
        &mut self,
        result: Result<OnboardingRunResult, String>,
    ) {
        self.onboarding.running = false;
        match result {
            Ok(result) => {
                self.status = if result.status.ready {
                    "Cookie is set up.".into()
                } else {
                    "Onboarding ran; the daemon still needs more.".into()
                };
                self.onboarding.summary = Some(run_summary(&result));
                self.onboarding.status = Some(result.status);
            }
            Err(error) => {
                self.status = "Not set up.".into();
                self.onboarding.error = Some(error);
            }
        }
    }

    fn default_root_agent(&self) -> Option<AgentId> {
        self.agents
            .iter()
            .find(|agent| {
                agent.id == self.default_agent && agent.preset.is_none() && agent.runnable_as_root
            })
            .map(|agent| agent.id.clone())
    }

    fn start_primary_session(&mut self) {
        let Some(agent) = self.default_root_agent() else {
            self.status = format!(
                "The default agent `{}` cannot start a session yet: run `cookie agents update`.",
                self.default_agent
            );
            return;
        };
        self.new_session_draft =
            self.draft_selection_for_preset(self.selected_preset.as_deref(), Some(&agent));
        self.modal = Modal::None;
        self.status = format!("New session on {agent}: type the first message.");
    }

    fn not_now(&mut self) {
        let missing = self
            .onboarding
            .status
            .as_ref()
            .map(|status| status.missing.clone());
        self.onboarding.remember_not_now(missing);
        if self.modal == Modal::Onboarding {
            self.modal = Modal::None;
        }
        self.status = "Not now: /onboard sets up Cookie any time.".into();
    }

    /// Hands step 1 over to the provider connect panel.
    fn connect_from_onboarding(&mut self) {
        self.onboarding.resume = true;
        self.clear_connect_secrets();
        self.modal = Modal::ConnectProviders;
        self.provider_search.reset();
        self.picker_state.select(Some(0));
        self.status = "Search providers, then press Down or Tab to choose one.".into();
    }

    /// Hands steps 2 and 3 over to the model picker.
    fn choose_model_from_onboarding(&mut self, slot: ModelSlot) {
        if self.models.is_empty() {
            self.status = "Connect a provider first: no model is available yet.".into();
            return;
        }
        self.onboarding.resume = true;
        self.onboarding.picking = Some(slot);
        self.model_search.reset();
        self.model_then_variant = true;
        self.picker_state.select(Some(0));
        self.modal = Modal::Models;
        self.status = match slot {
            ModelSlot::Default => "Choose the default model.".into(),
            ModelSlot::Small => "Choose the small model.".into(),
            ModelSlot::Utility => "Choose the utility model.".into(),
        };
    }

    /// The variant the picker highlights for an onboarding choice: the
    /// default and small models' own default, the utility model's first
    /// listed one.
    pub(in crate::ui) fn onboarding_preferred_variant(
        &self,
        slot: ModelSlot,
        model: &ModelKey,
    ) -> Option<VariantId> {
        let descriptor = self.model_descriptor(model)?;
        match slot {
            ModelSlot::Default => default_model_variant(descriptor),
            ModelSlot::Small => small_model_variant(descriptor),
            ModelSlot::Utility => utility_model_variant(descriptor),
        }
    }

    /// The picker chose `model` in `variant` for the panel.
    pub(in crate::ui) fn finish_onboarding_model(
        &mut self,
        slot: ModelSlot,
        model: ModelKey,
        variant: Option<VariantId>,
    ) {
        match slot {
            ModelSlot::Default => self.onboarding.default_choice = Some((model, variant)),
            ModelSlot::Small => self.onboarding.small_choice = Some((model, variant)),
            ModelSlot::Utility => self.onboarding.utility_choice = Some((model, variant)),
        }
        self.model_search.reset();
        self.model_then_variant = false;
        self.variant_step_model = None;
        self.modal = Modal::None;
        self.resume_onboarding();
        if let Some(next) = self.onboarding.step().next() {
            self.go_to_step(next);
        }
    }

    pub(in crate::ui) fn activate_onboarding_action(&mut self, action: OnboardingAction) {
        match action {
            OnboardingAction::SetUp => self.open_onboarding_panel(),
            OnboardingAction::NotNow => self.not_now(),
            OnboardingAction::Connect => self.connect_from_onboarding(),
            OnboardingAction::ChooseModel => match self.onboarding.step() {
                OnboardingStep::DefaultModel => {
                    self.choose_model_from_onboarding(ModelSlot::Default)
                }
                OnboardingStep::SmallModel => self.choose_model_from_onboarding(ModelSlot::Small),
                OnboardingStep::UtilityModel => {
                    self.choose_model_from_onboarding(ModelSlot::Utility)
                }
                OnboardingStep::Provider => {}
            },
            OnboardingAction::Back => {
                if let Some(step) = self.onboarding.step().previous() {
                    self.go_to_step(step);
                }
            }
            OnboardingAction::Next => {
                if let Some(step) = self.onboarding.step().next() {
                    self.go_to_step(step);
                }
            }
            OnboardingAction::Finish => self.finish_onboarding(),
            OnboardingAction::StartSession => self.start_primary_session(),
            OnboardingAction::Close => self.modal = Modal::None,
        }
    }

    /// The actions the panel offers now.
    pub(in crate::ui) fn onboarding_actions(&self) -> Vec<OnboardingAction> {
        if self.onboarding.summary.is_some() {
            let mut actions = Vec::new();
            if self.default_root_agent().is_some() {
                actions.push(OnboardingAction::StartSession);
            }
            actions.push(OnboardingAction::Close);
            return actions;
        }
        let step = self.onboarding.step();
        let mut actions = match step {
            OnboardingStep::Provider => vec![OnboardingAction::Connect],
            OnboardingStep::DefaultModel
            | OnboardingStep::SmallModel
            | OnboardingStep::UtilityModel => vec![OnboardingAction::ChooseModel],
        };
        if step.previous().is_some() {
            actions.push(OnboardingAction::Back);
        }
        if step.next().is_some() {
            actions.push(OnboardingAction::Next);
        }
        if self.onboarding_finish_refusal().is_none() {
            actions.push(OnboardingAction::Finish);
        }
        if self
            .onboarding
            .status
            .as_ref()
            .is_some_and(|status| !status.ready)
        {
            actions.push(OnboardingAction::NotNow);
        }
        actions.push(OnboardingAction::Close);
        actions
    }

    pub(super) fn handle_onboarding_key(&mut self, key: KeyEvent) {
        let offered = self.onboarding_actions();
        let offers = |action| offered.contains(&action);
        let action = match key.code {
            KeyCode::Esc => Some(OnboardingAction::Close),
            KeyCode::Left | KeyCode::BackTab => Some(OnboardingAction::Back),
            KeyCode::Right | KeyCode::Tab => Some(OnboardingAction::Next),
            KeyCode::Enter => offered.first().copied().filter(|action| {
                matches!(
                    action,
                    OnboardingAction::Connect
                        | OnboardingAction::ChooseModel
                        | OnboardingAction::StartSession
                )
            }),
            KeyCode::Char('n') if key.modifiers.is_empty() => Some(OnboardingAction::NotNow),
            KeyCode::Char('f') if key.modifiers == KeyModifiers::NONE => {
                if let Some(reason) = self.onboarding_finish_refusal() {
                    self.status = reason;
                }
                Some(OnboardingAction::Finish)
            }
            _ => None,
        };
        if let Some(action) = action.filter(|action| offers(*action)) {
            self.activate_onboarding_action(action);
        }
    }

    /// The notice, in the bar row above the composer: what is left, and its
    /// two buttons.
    pub(super) fn render_onboarding_notice(&mut self, frame: &mut Frame, area: Rect) {
        self.hit_map.onboarding.notice.clear();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let missing = self
            .onboarding
            .status
            .as_ref()
            .map(|status| {
                status
                    .missing
                    .iter()
                    .map(|requirement| rules::requirement_label(*requirement))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let text = format!("Cookie is not set up: {missing}. ");
        let mut spans = vec![Span::styled(text.clone(), self.theme.warning())];
        let mut x = area.x.saturating_add(
            u16::try_from(unicode_width::UnicodeWidthStr::width(text.as_str())).unwrap_or(u16::MAX),
        );
        for action in [OnboardingAction::SetUp, OnboardingAction::NotNow] {
            let label = format!(" {} ", action.label());
            let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
            if x.saturating_add(width) <= area.right() {
                self.hit_map
                    .onboarding
                    .notice
                    .push((Rect::new(x, area.y, width, 1), action));
            }
            spans.push(Span::styled(label, self.theme.link()));
            spans.push(Span::raw(" "));
            x = x.saturating_add(width + 1);
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(self.theme.panel()),
            area,
        );
    }

    pub(super) fn render_onboarding_panel(&mut self, frame: &mut Frame) {
        let area = centered(frame.area(), 86, 76);
        paint_panel(frame, area, &self.theme);
        let mut hits = OnboardingHits::default();
        let block = crate::ui::panel_block()
            .border_style(self.theme.panel_border())
            .title(crate::ui::panel_title("Set up Cookie"));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [tabs_area, body_area, reason_area, actions_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(inner);
        // The tabs, a check on each done step.
        let current = self.onboarding.step();
        let mut spans = Vec::new();
        let mut x = tabs_area.x;
        for step in OnboardingStep::ALL {
            let done = self
                .onboarding
                .status
                .as_ref()
                .is_some_and(|status| step.done(status))
                || self.chosen_for(step);
            let label = format!(" {} {} ", if done { "✓" } else { "·" }, step.label());
            let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
            if x.saturating_add(width) <= tabs_area.right() {
                hits.tabs.push((Rect::new(x, tabs_area.y, width, 1), step));
            }
            let style = if step == current {
                self.theme.selected()
            } else {
                self.theme.link()
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
            x = x.saturating_add(width + 1);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);
        // The body.
        let lines = self.onboarding_body(current);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);
        if let Some(reason) = self
            .onboarding_finish_refusal()
            .filter(|_| self.onboarding.summary.is_none())
        {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Finish: {reason}"),
                    self.theme.muted(),
                ))),
                reason_area,
            );
        }
        let mut spans = Vec::new();
        let mut x = actions_area.x;
        for action in self.onboarding_actions() {
            let label = format!(" {} ({}) ", action.label(), action.key_label());
            let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
            if x.saturating_add(width) <= actions_area.right() {
                hits.actions
                    .push((Rect::new(x, actions_area.y, width, 1), action));
            }
            spans.push(Span::styled(label, self.theme.link()));
            spans.push(Span::raw(" "));
            x = x.saturating_add(width + 1);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), actions_area);
        let notice = std::mem::take(&mut self.hit_map.onboarding.notice);
        self.hit_map.onboarding = OnboardingHits { notice, ..hits };
    }

    /// Whether the panel has a choice of its own for `step`.
    fn chosen_for(&self, step: OnboardingStep) -> bool {
        match step {
            OnboardingStep::DefaultModel => self.onboarding.default_choice.is_some(),
            OnboardingStep::SmallModel => self.onboarding.small_choice.is_some(),
            OnboardingStep::UtilityModel => self.onboarding.utility_choice.is_some(),
            OnboardingStep::Provider => false,
        }
    }

    fn onboarding_body(&self, step: OnboardingStep) -> Vec<Line<'static>> {
        let body = self.theme.body();
        let muted = self.theme.muted();
        let mut lines = Vec::new();
        if let Some(summary) = &self.onboarding.summary {
            lines.extend(summary.iter().map(|line| Line::styled(line.clone(), body)));
            return lines;
        }
        if let Some(error) = &self.onboarding.error {
            lines.push(Line::styled(error.clone(), self.theme.error()));
            lines.push(Line::raw(""));
        }
        let Some(status) = &self.onboarding.status else {
            lines.push(Line::styled("Asking the daemon…", muted));
            return lines;
        };
        match step {
            OnboardingStep::Provider => {
                if status.providers_connected.is_empty() {
                    lines.push(Line::styled("No provider is connected yet.", body));
                } else {
                    lines.push(Line::styled("Connected providers:", body));
                    for provider in &status.providers_connected {
                        let count = self
                            .models
                            .iter()
                            .filter(|model| &model.key.provider_id() == provider)
                            .count();
                        lines.push(Line::styled(
                            format!("  ✓ {provider} · {count} models available"),
                            body,
                        ));
                    }
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Enter connects a provider (API key or browser login); several may be connected.",
                    muted,
                ));
            }
            OnboardingStep::DefaultModel
            | OnboardingStep::SmallModel
            | OnboardingStep::UtilityModel => {
                let (slot, configured, about) = match step {
                    OnboardingStep::DefaultModel => (
                        ModelSlot::Default,
                        status.default_model.as_ref(),
                        "The model root agents reach as ${default_model}.",
                    ),
                    OnboardingStep::SmallModel => (
                        ModelSlot::Small,
                        status.small_model.as_ref(),
                        "Optional: the model subagents that carry out work, such as the official worker and explorer, reach as ${small_model}. Without it they use the default model.",
                    ),
                    _ => (
                        ModelSlot::Utility,
                        status.utility_model.as_ref(),
                        "Optional: a fast, cheap model for approvals and session titles, reached as ${utility_model}. Without it they use the session's own model.",
                    ),
                };
                lines.push(Line::styled(about, body));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!(
                        "Configured: {}",
                        configured.map_or_else(|| "not set".into(), model_label)
                    ),
                    body,
                ));
                if let Some((model, variant)) = self.onboarding.choice(slot) {
                    lines.push(Line::styled(
                        format!(
                            "Chosen: {model}{}",
                            variant
                                .as_ref()
                                .map_or_else(String::new, |variant| format!(" ({variant})"))
                        ),
                        self.theme.tool_success(),
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Enter opens the model list: type to search, then pick a variant.",
                    muted,
                ));
            }
        }
        lines
    }
}

#[cfg(test)]
pub(in crate::ui) fn sample_status(missing: &[OnboardingRequirement]) -> OnboardingStatus {
    OnboardingStatus {
        ready: missing.is_empty(),
        missing: missing.to_vec(),
        providers_connected: Vec::new(),
        default_model: None,
        small_model: None,
        utility_model: None,
    }
}
