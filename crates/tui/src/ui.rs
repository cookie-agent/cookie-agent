//! Ratatui presentation and terminal event loop.

use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookiecode_protocol::{
    AgentDescriptor, AgentListParams, ApprovalDecision, ApprovalRespondParams, Event,
    RunCancelParams, RunStartParams, RunSteerParams, RunToolStdinParams, SessionCreateParams,
    SessionId, SessionListParams, SessionMeta, SessionTree, SessionTreeParams,
};
use crossterm::{
    cursor::Show,
    event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    client::{Client, ClientDelivery},
    state::{ApprovalState, DeliveryOutcome, StateStore, ToolStatus, TranscriptItem},
};

enum Focus {
    Conversation,
    ToolStdin,
    Picker(Picker),
}

enum Picker {
    Sessions,
    Profiles,
}

/// UI state separated from the client and durable protocol projection.
pub struct App {
    client: Client,
    deliveries: Option<tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>>,
    pub store: StateStore,
    sessions: Vec<SessionMeta>,
    agents: Vec<AgentDescriptor>,
    tree: Option<SessionTree>,
    selected: Option<SessionId>,
    tree_selection: usize,
    collapsed: HashSet<SessionId>,
    picker_index: usize,
    input: String,
    focus: Focus,
    stdin_target: Option<cookiecode_protocol::ToolCallId>,
    status: String,
    should_quit: bool,
}

impl App {
    pub async fn new(client: Client) -> Self {
        // Subscribe before issuing events.subscribe so its replay and a live
        // tail racing App construction share the same retained receiver.
        let deliveries = client
            .subscribe_deliveries()
            .expect("app delivery receiver already attached");
        let mut app = Self {
            client,
            deliveries: Some(deliveries),
            store: StateStore::default(),
            sessions: Vec::new(),
            agents: Vec::new(),
            tree: None,
            selected: None,
            tree_selection: 0,
            collapsed: HashSet::new(),
            picker_index: 0,
            input: String::new(),
            focus: Focus::Conversation,
            stdin_target: None,
            status: "Connected. n: new session, s: sessions, q: quit".into(),
            should_quit: false,
        };
        app.refresh_lists().await;
        if let Some(session_id) = app.sessions.first().map(|session| session.id) {
            app.select_session(session_id).await;
            app.drain_replay(session_id).await;
            app.refresh_tree().await;
        }
        app
    }

    fn take_deliveries(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<ClientDelivery> {
        self.deliveries
            .take()
            .expect("app delivery receiver already attached")
    }

    async fn drain_replay(&mut self, session_id: SessionId) {
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

    async fn refresh_lists(&mut self) {
        match self
            .client
            .list_sessions(SessionListParams::default())
            .await
        {
            Ok(result) => self.sessions = result.sessions,
            Err(error) => self.status = error.to_string(),
        }
        match self.client.list_agents(AgentListParams::default()).await {
            Ok(result) => self.agents = result.agents,
            Err(error) => self.status = error.to_string(),
        }
    }

    async fn select_session(&mut self, session_id: SessionId) {
        self.selected = Some(session_id);
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        match self.client.subscribe_events(session_id, cursor).await {
            Ok(()) => {}
            Err(error) => {
                self.status = error.to_string();
            }
        }
    }

    async fn refresh_tree(&mut self) {
        if let Some(session_id) = self.selected {
            match self
                .client
                .session_tree(SessionTreeParams { session_id })
                .await
            {
                Ok(result) => self.tree = Some(result.tree),
                Err(error) => self.status = error.to_string(),
            }
        }
    }

    async fn handle_delivery(&mut self, delivery: ClientDelivery) {
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
                cookiecode_protocol::EventSubscriptionMessage::Event {
                    event: cookiecode_protocol::EventEnvelope {
                        event: Event::ToolCallLinked { .. },
                        ..
                    }
                }
            ),
            ClientDelivery::ReplayEvent { event, .. } => {
                matches!(event.event, Event::ToolCallLinked { .. })
            }
            _ => false,
        };
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
        if linked || replay_finished {
            self.refresh_tree().await;
        }
    }

    fn recover_timed_out_replays(&mut self) {
        for session_id in self.store.abandon_timed_out_replays() {
            self.status = "replay timed out; retrying recovery".into();
            self.client.recover_session(session_id, true);
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        if matches!(self.focus, Focus::Conversation)
            && self.input.is_empty()
            && self.current_approval().is_none()
            && key.code == KeyCode::Char('q')
            && key.modifiers.is_empty()
        {
            self.should_quit = true;
            return;
        }
        match self.focus {
            Focus::Picker(Picker::Sessions) => self.handle_session_picker(key).await,
            Focus::Picker(Picker::Profiles) => self.handle_profile_picker(key).await,
            Focus::ToolStdin => self.handle_stdin_key(key).await,
            Focus::Conversation => self.handle_conversation_key(key).await,
        }
    }

    async fn handle_conversation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('n') if self.input.is_empty() => {
                self.focus = Focus::Picker(Picker::Profiles);
                self.picker_index = 0;
            }
            KeyCode::Char('s') if self.input.is_empty() => {
                self.focus = Focus::Picker(Picker::Sessions);
                self.picker_index = 0;
            }
            KeyCode::Char('i') if self.input.is_empty() => {
                if self.select_next_stdin_target() {
                    self.focus = Focus::ToolStdin;
                    self.status = format!(
                        "tool stdin for {} (Tab: next call, Ctrl-D: EOF)",
                        self.stdin_target.expect("target selected")
                    );
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_active_run().await;
            }
            KeyCode::Char('j') if self.input.is_empty() => {
                self.tree_selection =
                    (self.tree_selection + 1).min(self.tree_entries().len().saturating_sub(1));
            }
            KeyCode::Char('k') if self.input.is_empty() => {
                self.tree_selection = self.tree_selection.saturating_sub(1);
            }
            KeyCode::Char('e') if self.input.is_empty() => {
                if let Some(session_id) = self
                    .tree_entries()
                    .get(self.tree_selection)
                    .map(|(session_id, _)| *session_id)
                    && !self.collapsed.insert(session_id)
                {
                    self.collapsed.remove(&session_id);
                }
            }
            KeyCode::Char('w') if self.input.is_empty() => {
                if let Some(session_id) = self
                    .tree_entries()
                    .get(self.tree_selection)
                    .map(|(session_id, _)| *session_id)
                {
                    self.select_session(session_id).await;
                }
            }
            KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3')
                if self.input.is_empty() =>
            {
                let decision = match key.code {
                    KeyCode::Char('1') => ApprovalDecision::Once,
                    KeyCode::Char('2') => ApprovalDecision::Always,
                    _ => ApprovalDecision::Reject,
                };
                self.answer_approval(decision).await;
            }
            KeyCode::Enter => self.submit_input().await,
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.push(character)
            }
            _ => {}
        }
    }

    async fn handle_stdin_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            self.send_stdin(true).await;
            self.focus = Focus::Conversation;
            return;
        }
        match key.code {
            KeyCode::Tab => {
                if self.select_next_stdin_target() {
                    self.status = format!(
                        "tool stdin for {} (Tab: next call, Ctrl-D: EOF)",
                        self.stdin_target.expect("target selected")
                    );
                }
            }
            KeyCode::Esc => self.focus = Focus::Conversation,
            KeyCode::Enter => self.send_stdin(false).await,
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.push(character)
            }
            _ => {}
        }
    }

    async fn handle_session_picker(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.focus = Focus::Conversation,
            KeyCode::Up => self.picker_index = self.picker_index.saturating_sub(1),
            KeyCode::Down => {
                self.picker_index =
                    (self.picker_index + 1).min(self.sessions.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(session) = self.sessions.get(self.picker_index) {
                    let session_id = session.id;
                    self.focus = Focus::Conversation;
                    self.select_session(session_id).await;
                }
            }
            _ => {}
        }
    }

    async fn handle_profile_picker(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.focus = Focus::Conversation,
            KeyCode::Up => self.picker_index = self.picker_index.saturating_sub(1),
            KeyCode::Down => {
                self.picker_index = (self.picker_index + 1).min(self.agents.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(agent) = self.agents.get(self.picker_index) {
                    let profile = agent.name.clone();
                    self.focus = Focus::Conversation;
                    let cwd = current_dir();
                    match self
                        .client
                        .create_session(SessionCreateParams { cwd, profile })
                        .await
                    {
                        Ok(result) => {
                            self.sessions.push(result.session.clone());
                            self.select_session(result.session.id).await;
                        }
                        Err(error) => self.status = error.to_string(),
                    }
                }
            }
            _ => {}
        }
    }

    async fn submit_input(&mut self) {
        let Some(session_id) = self.selected else {
            self.status = "create or select a session first".into();
            return;
        };
        if self.input.trim().is_empty() {
            return;
        }
        let input = std::mem::take(&mut self.input);
        let result = if let Some(run_id) = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.active_run)
        {
            self.client
                .steer_run(RunSteerParams { run_id, input })
                .await
                .map(|_| ())
        } else {
            self.client
                .start_run(RunStartParams {
                    session_id,
                    client_run_id: client_run_id(),
                    input,
                })
                .await
                .map(|_| ())
        };
        if let Err(error) = result {
            self.status = error.to_string();
        }
    }

    async fn send_stdin(&mut self, eof: bool) {
        let Some((run_id, call_id)) = self.selected_running_tool() else {
            self.status = "no running interactive tool".into();
            return;
        };
        let input = std::mem::take(&mut self.input);
        let data = (!input.is_empty()).then(|| STANDARD.encode(input.as_bytes()));
        match self
            .client
            .tool_stdin(RunToolStdinParams {
                run_id,
                call_id,
                data,
                eof,
            })
            .await
        {
            Ok(result) if !result.accepted => self.status = "stdin was rejected by the tool".into(),
            Ok(_) => {
                self.status = if eof {
                    "tool stdin closed".into()
                } else {
                    "stdin sent".into()
                }
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    async fn answer_approval(&mut self, decision: ApprovalDecision) {
        let Some(approval) = self.current_approval().cloned() else {
            return;
        };
        let scope =
            (decision == ApprovalDecision::Always).then(|| approval.suggested_pattern.clone());
        match self
            .client
            .respond_approval(ApprovalRespondParams {
                session_id: approval.session_id,
                approval_id: approval.approval_id,
                decision,
                scope,
                feedback: None,
            })
            .await
        {
            Ok(_) => self.status = "approval sent".into(),
            Err(error) => self.status = error.to_string(),
        }
    }

    fn current_approval(&self) -> Option<&ApprovalState> {
        self.selected
            .and_then(|id| self.store.sessions.get(&id))
            .and_then(|state| state.approvals.first())
    }

    fn selected_running_tool(
        &mut self,
    ) -> Option<(cookiecode_protocol::RunId, cookiecode_protocol::ToolCallId)> {
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

    fn running_tool_ids(&self) -> Vec<cookiecode_protocol::ToolCallId> {
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

    fn select_next_stdin_target(&mut self) -> bool {
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

    async fn cancel_active_run(&mut self) {
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
        match self.client.cancel_run(RunCancelParams { run_id }).await {
            Ok(result) if result.cancelled => self.status = "run cancellation requested".into(),
            Ok(_) => self.status = "run was already complete".into(),
            Err(error) => self.status = error.to_string(),
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(frame.area());
        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
            .split(outer[0]);
        self.render_tree(frame, main[0]);
        self.render_conversation(frame, main[1]);
        let title = match self.focus {
            Focus::ToolStdin => "Tool stdin (Tab cycles running calls)",
            _ => "Message (Enter to send; i: tool stdin)",
        };
        frame.render_widget(
            Paragraph::new(self.input.as_str())
                .block(Block::default().borders(Borders::ALL).title(title)),
            outer[1],
        );
        frame.render_widget(
            Paragraph::new(self.status.as_str()).style(Style::default().fg(Color::DarkGray)),
            outer[2],
        );
        if let Some(approval) = self.current_approval() {
            self.render_approval(frame, approval, centered(frame.area(), 76, 40));
        }
        match self.focus {
            Focus::Picker(Picker::Sessions) => self.render_picker(
                frame,
                "Sessions",
                self.sessions
                    .iter()
                    .map(|s| format!("{}  {}", s.id, s.profile.name))
                    .collect(),
                centered(frame.area(), 68, 50),
            ),
            Focus::Picker(Picker::Profiles) => self.render_picker(
                frame,
                "New session profile",
                self.agents.iter().map(|a| a.name.clone()).collect(),
                centered(frame.area(), 50, 40),
            ),
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn draw_for_test(&self, frame: &mut ratatui::Frame) {
        self.draw(frame);
    }

    fn render_tree(&self, frame: &mut ratatui::Frame, area: Rect) {
        let entries: Vec<String> = self
            .tree_entries()
            .into_iter()
            .enumerate()
            .map(|(index, (_, entry))| {
                let marker = if index == self.tree_selection {
                    "> "
                } else {
                    "  "
                };
                format!("{marker}{entry}")
            })
            .collect();
        if entries.is_empty() {
            // There are no selectable nodes until a session has been loaded.
            frame.render_widget(
                List::new(vec!["No session selected"]).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Session tree (j/k: select, e: expand, w: watch)"),
                ),
                area,
            );
            return;
        }
        frame.render_widget(
            List::new(entries).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Session tree (j/k: select, e: expand, w: watch)"),
            ),
            area,
        );
    }

    fn tree_entries(&self) -> Vec<(SessionId, String)> {
        let mut entries = Vec::new();
        if let Some(tree) = &self.tree {
            flatten_tree(tree, 0, &self.collapsed, &mut entries);
        }
        entries
    }

    fn render_conversation(&self, frame: &mut ratatui::Frame, area: Rect) {
        let lines = self
            .selected
            .and_then(|id| self.store.sessions.get(&id))
            .map(|state| transcript_lines(state))
            .unwrap_or_else(|| vec![Line::from("Select or create a session")]);
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("Conversation")),
            area,
        );
    }

    fn render_approval(&self, frame: &mut ratatui::Frame, approval: &ApprovalState, area: Rect) {
        frame.render_widget(Clear, area);
        let resources = approval
            .resources
            .iter()
            .map(|resource| {
                format!(
                    "- {:?}: {} ({})",
                    resource.action, resource.resource, resource.suggested_pattern
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!(
            "Permission required\n\ntool action: {}\nresource: {}\nsuggested always pattern: {}\nall asking resources:\n{}\nreason: {}\n\n[1] once   [2] always   [3] reject",
            approval.action,
            approval.resource,
            approval.suggested_pattern,
            resources,
            approval.trace.precedence_reason,
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Approval")
                    .style(Style::default().fg(Color::Yellow)),
            ),
            area,
        );
    }

    fn render_picker(
        &self,
        frame: &mut ratatui::Frame,
        title: &str,
        entries: Vec<String>,
        area: Rect,
    ) {
        frame.render_widget(Clear, area);
        let items: Vec<ListItem> = entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let marker = if index == self.picker_index {
                    "> "
                } else {
                    "  "
                };
                ListItem::new(format!("{marker}{entry}"))
            })
            .collect();
        frame.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }
}

/// Run the terminal UI against a connected client.
pub async fn run_with_client(client: Client) -> anyhow::Result<()> {
    let mut restore = TerminalRestore::default();
    enable_raw_mode().context("enable terminal raw mode")?;
    restore.raw_mode = true;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    restore.alternate_screen = true;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    let mut app = App::new(client).await;
    let deliveries = app.take_deliveries();
    let result = event_loop(&mut terminal, app, deliveries).await;
    drop(terminal);
    drop(restore);
    result
}

#[derive(Default)]
struct TerminalRestore {
    raw_mode: bool,
    alternate_screen: bool,
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.alternate_screen {
            let _ = execute!(stdout, LeaveAlternateScreen, Show);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    mut deliveries: tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut replay_watchdog = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        terminal
            .draw(|frame| app.draw(frame))
            .context("draw terminal")?;
        if app.should_quit {
            return Ok(());
        }
        tokio::select! {
            Some(event) = events.next() => match event {
                Ok(CrosstermEvent::Key(key)) => app.handle_key(key).await,
                Ok(_) => {},
                Err(error) => app.status = error.to_string(),
            },
            delivery = deliveries.recv() => match delivery {
                Some(delivery) => app.handle_delivery(delivery).await,
                None => {
                    for session_id in app.store.abandon_replays() {
                        app.client.recover_session(session_id, true);
                    }
                    app.status = "daemon disconnected".into();
                    return Ok(());
                }
            },
            _ = replay_watchdog.tick() => app.recover_timed_out_replays(),
        }
    }
}

fn transcript_lines(state: &crate::state::SessionState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in &state.transcript {
        match item {
            TranscriptItem::User(text) => lines.push(Line::from(vec![
                Span::styled("You: ", Style::default().fg(Color::Cyan)),
                Span::raw(text.clone()),
            ])),
            TranscriptItem::Assistant(text) => lines.push(Line::from(vec![
                Span::styled("Assistant: ", Style::default().fg(Color::Green)),
                Span::raw(text.clone()),
            ])),
            TranscriptItem::Reasoning(text) => lines.push(Line::from(vec![
                Span::styled("Reasoning: ", Style::default().fg(Color::DarkGray)),
                Span::raw(text.clone()),
            ])),
            TranscriptItem::Status(text) => lines.push(Line::from(format!("• {text}"))),
            TranscriptItem::Tool(id) => {
                if let Some(tool) = state.tools.get(id) {
                    lines.push(Line::from(format!(
                        "[tool {}: {:?}] {}",
                        tool.tool, tool.status, tool.arguments
                    )));
                    if !tool.detail.is_empty() {
                        lines.push(Line::from(format!("  {}", tool.detail)));
                    }
                    for (stderr, label) in [(false, "stdout"), (true, "stderr")] {
                        if let Some(output) = state.output.get(&(*id, stderr)) {
                            let gap = if output.has_gap { " (output gap)" } else { "" };
                            lines.push(Line::from(format!("  {label}{gap}: {}", output.text())));
                        }
                    }
                }
            }
        }
    }
    lines
}

fn flatten_tree(
    tree: &SessionTree,
    depth: usize,
    collapsed: &HashSet<SessionId>,
    entries: &mut Vec<(SessionId, String)>,
) {
    let marker = if tree.children.is_empty() {
        " "
    } else if collapsed.contains(&tree.session.id) {
        "+"
    } else {
        "-"
    };
    entries.push((
        tree.session.id,
        format!(
            "{}{} {} ({})",
            "  ".repeat(depth),
            marker,
            tree.session.profile.name,
            tree.session.id
        ),
    ));
    if !collapsed.contains(&tree.session.id) {
        for child in &tree.children {
            flatten_tree(child, depth + 1, collapsed, entries);
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

fn current_dir() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string()
}

fn client_run_id() -> String {
    let ticks = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("tui-{ticks}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use cookiecode_server::{MessageFrame, MessageStream, TransportError};

    use super::*;
    use crate::state::{SessionState, ToolCallState};

    struct NeverStream;

    #[async_trait]
    impl MessageStream for NeverStream {
        async fn send(&mut self, _: MessageFrame) -> Result<(), TransportError> {
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn completed_stdin_target_advances_to_another_running_tool() {
        let session_id = SessionId::new_v7();
        let run_id = cookiecode_protocol::RunId::new_v7();
        let completed = cookiecode_protocol::ToolCallId::new_v7();
        let running = cookiecode_protocol::ToolCallId::new_v7();
        let mut state = SessionState {
            active_run: Some(run_id),
            ..SessionState::default()
        };
        state.tools = HashMap::from([
            (
                completed,
                ToolCallState {
                    id: completed,
                    tool: "bash".into(),
                    arguments: "{}".into(),
                    status: ToolStatus::Completed,
                    detail: String::new(),
                },
            ),
            (
                running,
                ToolCallState {
                    id: running,
                    tool: "bash".into(),
                    arguments: "{}".into(),
                    status: ToolStatus::Running,
                    detail: String::new(),
                },
            ),
        ]);
        let mut store = StateStore::default();
        store.sessions.insert(session_id, state);
        let mut app = App {
            client: Client::connect_stream(NeverStream),
            deliveries: None,
            store,
            sessions: Vec::new(),
            agents: Vec::new(),
            tree: None,
            selected: Some(session_id),
            tree_selection: 0,
            collapsed: HashSet::new(),
            picker_index: 0,
            input: String::new(),
            focus: Focus::ToolStdin,
            stdin_target: Some(completed),
            status: String::new(),
            should_quit: false,
        };
        assert_eq!(app.selected_running_tool(), Some((run_id, running)));
        assert_eq!(app.stdin_target, Some(running));
    }
}
