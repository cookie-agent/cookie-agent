//! User-owned goal commands, persistent summary bar, and read-only details.

use cookie_agent_protocol::{
    GoalLifecycleAction, GoalState, GoalStatus, SessionGoalGetParams, SessionGoalLifecycleParams,
    SessionGoalSetParams, SessionId,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::{
    App, GoalBarAction, MAX_TRANSIENT_NOTICES, Modal, RpcUpdate, paint_panel,
    truncate_with_ellipsis,
};
use crate::ui::slash::GoalCommand;
use crate::ui::transcript::wrapped_line;

#[derive(Debug, Default)]
pub(super) struct GoalDetailState {
    session_id: Option<SessionId>,
    scroll: usize,
    max_scroll: usize,
    page_size: usize,
}

impl App {
    pub(super) fn run_goal_command(&mut self, command: GoalCommand) {
        let Some(session_id) = self.selected else {
            self.status = "select a root session before using /goal".into();
            return;
        };
        if !self.watching_root_session() {
            self.status = "goal commands are only available in root sessions".into();
            return;
        }
        if self.read_only_sessions.contains(&session_id) {
            self.status = "cannot change a goal in a read-only session".into();
            return;
        }
        if matches!(&command, GoalCommand::Objective(objective) if objective.trim().is_empty()) {
            self.status = "goal objective must not be empty".into();
            return;
        }
        let selection = if matches!(command, GoalCommand::Objective(_) | GoalCommand::Resume) {
            let Some(selection) = self.validated_draft_selection() else {
                self.status =
                    "select a draft agent/model before activating or resuming a goal".into();
                return;
            };
            Some(selection)
        } else {
            None
        };
        self.status = "updating goal...".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = async {
                match command {
                    GoalCommand::Objective(objective) => client
                        .set_session_goal(SessionGoalSetParams {
                            session_id,
                            objective,
                            selection,
                        })
                        .await
                        .map(|result| Some(result.goal))
                        .map_err(|error| error.to_string()),
                    control => {
                        let action = match control {
                            GoalCommand::Pause => GoalLifecycleAction::Pause,
                            GoalCommand::Resume => GoalLifecycleAction::Resume,
                            GoalCommand::Cancel => GoalLifecycleAction::Cancel,
                            GoalCommand::Objective(_) => unreachable!(),
                        };
                        let goal = client
                            .get_session_goal(SessionGoalGetParams { session_id })
                            .await
                            .map_err(|error| error.to_string())?
                            .goal
                            .ok_or_else(|| "no goal is set for this session".to_owned())?;
                        client
                            .change_session_goal_lifecycle(SessionGoalLifecycleParams {
                                session_id,
                                goal_id: goal.goal_id,
                                expected_revision: goal.revision,
                                action,
                                selection,
                            })
                            .await
                            .map(|result| Some(result.goal))
                            .map_err(|error| error.to_string())
                    }
                }
            }
            .await;
            let _ = updates.send(RpcUpdate::GoalFinished {
                session_id,
                result: Box::new(result),
            });
        });
    }

    pub(super) fn finish_goal_command(
        &mut self,
        session_id: SessionId,
        result: Result<Option<GoalState>, String>,
    ) {
        let status = match result {
            Ok(goal) => goal.as_ref().map_or_else(
                || "no goal is set for this session".to_owned(),
                |goal| format!("goal {}", status_name(goal.status)),
            ),
            Err(error) => {
                let message = format!("goal command failed: {error}");
                self.session_errors.record(&message);
                self.push_goal_notice(session_id, message.clone());
                message
            }
        };
        // Only durable events update the projection. An RPC response may arrive
        // after a newer event or a revert and must not overwrite that state.
        if self.selected == Some(session_id) {
            self.status = status;
        }
    }

    pub(super) fn goal_bar_visible(&self) -> bool {
        self.projected_goal().is_some()
    }

    pub(super) fn render_goal_bar(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.hit_map.goal_actions.clear();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(goal) = self.projected_goal().cloned() else {
            return;
        };

        let actions = self.allowed_goal_actions();
        if self
            .goal_focus
            .is_some_and(|focused| !actions.contains(&focused))
        {
            self.goal_focus = Some(GoalBarAction::Details);
        }
        let controls = bar_controls(&actions, area.width);
        let controls_width = controls
            .iter()
            .map(|(_, label)| UnicodeWidthStr::width(*label))
            .sum::<usize>()
            .min(usize::from(area.width));
        let details_width = usize::from(area.width).saturating_sub(controls_width);
        let finished = goal.items.iter().filter(|item| item.finished).count();
        let summary = format!(
            "{} | {} {finished}/{}",
            single_line(&goal.objective),
            status_name(goal.status),
            goal.items.len(),
        );
        let mut details = truncate_with_ellipsis(&summary, details_width);
        details.push_str(
            &" ".repeat(details_width.saturating_sub(UnicodeWidthStr::width(details.as_str()))),
        );
        let details_style =
            action_style(&self.theme, self.goal_focus == Some(GoalBarAction::Details));
        let mut spans = vec![Span::styled(details, details_style)];
        if details_width > 0 {
            self.hit_map.goal_actions.push((
                Rect::new(area.x, area.y, details_width as u16, 1),
                GoalBarAction::Details,
            ));
        }
        let mut x = area.x.saturating_add(details_width as u16);
        for (action, label) in controls {
            let width = UnicodeWidthStr::width(label).min(usize::from(u16::MAX)) as u16;
            spans.push(Span::styled(
                label,
                action_style(&self.theme, self.goal_focus == Some(action)),
            ));
            self.hit_map
                .goal_actions
                .push((Rect::new(x, area.y, width, 1), action));
            x = x.saturating_add(width);
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(self.theme.panel()),
            area,
        );
    }

    pub(super) fn open_goal_detail(&mut self) {
        let Some(session_id) = self.selected.filter(|_| self.projected_goal().is_some()) else {
            self.status = "no goal is set for this root session".into();
            return;
        };
        self.goal_detail = GoalDetailState {
            session_id: Some(session_id),
            ..Default::default()
        };
        self.modal = Modal::GoalDetail;
    }

    pub(super) fn activate_goal_action(&mut self, action: GoalBarAction) {
        if action == GoalBarAction::Details {
            self.open_goal_detail();
            return;
        }
        let Some(session_id) = self.selected else {
            self.status = "select a root session before changing a goal".into();
            return;
        };
        if !self.watching_root_session() {
            self.status = "goal actions are only available in root sessions".into();
            return;
        }
        if self.read_only_sessions.contains(&session_id) {
            self.status = "cannot change a goal in a read-only session".into();
            return;
        }
        let Some(goal) = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.goal.as_ref())
        else {
            self.status = "no goal is set for this session".into();
            return;
        };
        let valid = matches!(
            (goal.status, action),
            (
                GoalStatus::Active,
                GoalBarAction::Pause | GoalBarAction::Cancel
            ) | (
                GoalStatus::Paused,
                GoalBarAction::Resume | GoalBarAction::Cancel
            )
        );
        if !valid {
            self.status = format!(
                "{} is unavailable while the goal is {}",
                goal_action_label(action).to_ascii_lowercase(),
                status_name(goal.status)
            );
            return;
        }
        self.run_goal_command(match action {
            GoalBarAction::Pause => GoalCommand::Pause,
            GoalBarAction::Resume => GoalCommand::Resume,
            GoalBarAction::Cancel => GoalCommand::Cancel,
            GoalBarAction::Details => unreachable!(),
        });
    }

    pub(super) fn cycle_goal_focus(&mut self, backwards: bool) {
        let actions = self.allowed_goal_actions();
        if actions.is_empty() {
            self.goal_focus = None;
            return;
        }
        let current = self
            .goal_focus
            .and_then(|focused| actions.iter().position(|action| *action == focused));
        let next = match current {
            None => 0,
            Some(0) if backwards => actions.len() - 1,
            Some(index) if backwards => index - 1,
            Some(index) => (index + 1) % actions.len(),
        };
        self.goal_focus = Some(actions[next]);
        self.status = format!("Goal: {}", goal_action_label(actions[next]));
    }

    pub(super) fn handle_goal_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => self.close_goal_detail(),
            KeyCode::Up => self.scroll_goal_detail(true),
            KeyCode::Down => self.scroll_goal_detail(false),
            KeyCode::PageUp => {
                self.goal_detail.scroll = self
                    .goal_detail
                    .scroll
                    .saturating_sub(self.goal_detail.page_size.max(1));
            }
            KeyCode::PageDown => {
                self.goal_detail.scroll = self
                    .goal_detail
                    .scroll
                    .saturating_add(self.goal_detail.page_size.max(1))
                    .min(self.goal_detail.max_scroll);
            }
            KeyCode::Home => self.goal_detail.scroll = 0,
            KeyCode::End => self.goal_detail.scroll = self.goal_detail.max_scroll,
            KeyCode::Enter => self.close_goal_detail(),
            _ => {}
        }
    }

    pub(super) fn scroll_goal_detail(&mut self, up: bool) {
        self.goal_detail.scroll = if up {
            self.goal_detail.scroll.saturating_sub(1)
        } else {
            self.goal_detail
                .scroll
                .saturating_add(1)
                .min(self.goal_detail.max_scroll)
        };
    }

    pub(super) fn render_goal_detail(&mut self, frame: &mut ratatui::Frame) {
        let Some(goal) = self.detail_goal().cloned() else {
            self.close_goal_detail();
            return;
        };
        let frame_area = frame.area();
        if frame_area.width == 0 || frame_area.height == 0 {
            self.hit_map.goal_close = None;
            return;
        }
        let width = frame_area.width.min(78);
        let height = frame_area.height.min(30);
        let area = Rect::new(
            frame_area.x.saturating_add((frame_area.width - width) / 2),
            frame_area
                .y
                .saturating_add((frame_area.height - height) / 2),
            width,
            height,
        );
        paint_panel(frame, area, &self.theme);
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        let finished = goal.items.iter().filter(|item| item.finished).count();
        let mut lines = goal
            .objective
            .split('\n')
            .flat_map(|line| {
                wrapped_line(
                    Line::styled(
                        line.trim_end_matches('\r').to_owned(),
                        self.theme.assistant(),
                    ),
                    inner.width,
                )
            })
            .collect::<Vec<_>>();
        lines.extend(wrapped_line(
            Line::styled(
                format!(
                    "status: {} | {finished}/{} finished | read-only",
                    status_name(goal.status),
                    goal.items.len()
                ),
                self.theme.internal(),
            ),
            inner.width,
        ));
        lines.push(Line::default());
        if goal.items.is_empty() {
            lines.push(Line::styled("Checklist is empty.", self.theme.muted()));
        } else {
            for (index, item) in goal.items.iter().enumerate() {
                let marker = if item.finished { "[x]" } else { "[ ]" };
                for (line_index, line) in item.description.lines().enumerate() {
                    let text = if line_index == 0 {
                        format!("{}. {marker} {line}", index + 1)
                    } else {
                        line.to_owned()
                    };
                    lines.extend(wrapped_line(
                        Line::styled(text, self.theme.body()),
                        inner.width,
                    ));
                }
            }
        }
        self.goal_detail.page_size = usize::from(inner.height);
        self.goal_detail.max_scroll = lines.len().saturating_sub(self.goal_detail.page_size);
        self.goal_detail.scroll = self.goal_detail.scroll.min(self.goal_detail.max_scroll);
        let visible_start = self
            .goal_detail
            .scroll
            .saturating_add(1)
            .min(lines.len().max(1));
        let visible_end = self
            .goal_detail
            .scroll
            .saturating_add(self.goal_detail.page_size)
            .min(lines.len());
        let title = if area.width < 42 {
            format!("Goal {visible_start}-{visible_end}/{}", lines.len())
        } else {
            format!(
                "Goal details | {visible_start}-{visible_end}/{} | Up/Down PgUp/PgDn Home/End",
                lines.len()
            )
        };
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((self.goal_detail.scroll.min(usize::from(u16::MAX)) as u16, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(self.theme.panel_border())
                        .title(Span::styled(
                            truncate_with_ellipsis(
                                &title,
                                usize::from(area.width.saturating_sub(2)),
                            ),
                            self.theme.heading(),
                        ))
                        .title_bottom(
                            Line::from(Span::styled(
                                truncate_with_ellipsis(
                                    "Enter/Esc: close",
                                    usize::from(area.width.saturating_sub(2)),
                                ),
                                self.theme.internal(),
                            ))
                            .right_aligned(),
                        ),
                ),
            area,
        );
        let close_width = "Enter/Esc: close"
            .len()
            .min(usize::from(area.width.saturating_sub(2))) as u16;
        self.hit_map.goal_close = (close_width > 0).then(|| {
            Rect::new(
                area.right().saturating_sub(1).saturating_sub(close_width),
                area.bottom().saturating_sub(1),
                close_width,
                1,
            )
        });
    }

    fn projected_goal(&self) -> Option<&GoalState> {
        self.selected
            .filter(|_| self.watching_root_session())
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .and_then(|state| state.goal.as_ref())
    }

    fn detail_goal(&self) -> Option<&GoalState> {
        let session_id = self.goal_detail.session_id?;
        (self.modal == Modal::GoalDetail && self.selected == Some(session_id))
            .then(|| self.projected_goal())
            .flatten()
    }

    fn allowed_goal_actions(&self) -> Vec<GoalBarAction> {
        let Some(goal) = self.projected_goal() else {
            return Vec::new();
        };
        let mut actions = vec![GoalBarAction::Details];
        if self
            .selected
            .is_some_and(|session_id| self.read_only_sessions.contains(&session_id))
        {
            return actions;
        }
        match goal.status {
            GoalStatus::Active => actions.extend([GoalBarAction::Pause, GoalBarAction::Cancel]),
            GoalStatus::Paused => actions.extend([GoalBarAction::Resume, GoalBarAction::Cancel]),
            GoalStatus::Completed | GoalStatus::Cancelled => {}
        }
        actions
    }

    fn close_goal_detail(&mut self) {
        self.modal = Modal::None;
        self.goal_detail = GoalDetailState::default();
        self.hit_map.goal_close = None;
    }

    fn push_goal_notice(&mut self, session_id: SessionId, notice: String) {
        let notices = self.goal_notices.entry(session_id).or_default();
        notices.push(notice);
        if notices.len() > MAX_TRANSIENT_NOTICES {
            notices.drain(..notices.len() - MAX_TRANSIENT_NOTICES);
        }
    }

    pub(super) fn notify_goal_completed(
        &mut self,
        session_id: SessionId,
        goal_id: cookie_agent_protocol::GoalId,
        revision: u64,
    ) {
        if let Some(goal) = self
            .store
            .sessions
            .get(&session_id)
            .and_then(|state| state.goal.as_ref())
            .filter(|goal| {
                goal.status == GoalStatus::Completed
                    && goal.goal_id == goal_id
                    && goal.revision == revision
            })
        {
            let notice = format!("Goal completed: {}", goal.objective);
            if self.selected == Some(session_id) {
                self.status = notice;
            } else {
                self.push_goal_notice(session_id, notice);
            }
        }
    }
}

fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
    }
}

pub(super) fn goal_action_label(action: GoalBarAction) -> &'static str {
    match action {
        GoalBarAction::Details => "Details",
        GoalBarAction::Pause => "Pause",
        GoalBarAction::Resume => "Resume",
        GoalBarAction::Cancel => "Cancel",
    }
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn action_style(theme: &crate::theme::Theme, focused: bool) -> Style {
    if focused {
        theme.selected()
    } else {
        theme.link()
    }
}

fn bar_controls(actions: &[GoalBarAction], width: u16) -> Vec<(GoalBarAction, &'static str)> {
    let lifecycle = actions
        .iter()
        .copied()
        .filter(|action| *action != GoalBarAction::Details)
        .collect::<Vec<_>>();
    if lifecycle.is_empty() {
        return Vec::new();
    }
    let full = lifecycle
        .iter()
        .map(|action| {
            let label = match action {
                GoalBarAction::Pause => " [Pause]",
                GoalBarAction::Resume => " [Resume]",
                GoalBarAction::Cancel => " [Cancel]",
                GoalBarAction::Details => unreachable!(),
            };
            (*action, label)
        })
        .collect::<Vec<_>>();
    let full_width = full
        .iter()
        .map(|(_, label)| UnicodeWidthStr::width(*label))
        .sum::<usize>();
    if usize::from(width) >= full_width.saturating_add(8) {
        return full;
    }
    if width >= 7 {
        return lifecycle
            .into_iter()
            .map(|action| {
                let label = match action {
                    GoalBarAction::Pause => " ||",
                    GoalBarAction::Resume => " >",
                    GoalBarAction::Cancel => " x",
                    GoalBarAction::Details => unreachable!(),
                };
                (action, label)
            })
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use cookie_agent_protocol::{
        GoalId, GoalItem, MessageFrame, MessageStream, SessionMeta, SessionOrigin, TransportError,
    };
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use super::*;
    use crate::ui::slash::{Submission, parse_submission};
    use crate::{client::Client, config::TuiConfig, state::SessionState, theme::Theme};

    struct ScriptedStream {
        incoming: mpsc::UnboundedReceiver<MessageFrame>,
        sent: mpsc::UnboundedSender<MessageFrame>,
    }

    #[async_trait]
    impl MessageStream for ScriptedStream {
        async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
            self.sent.send(frame).map_err(|_| TransportError::Closed)
        }

        async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
            Ok(self.incoming.recv().await)
        }
    }

    async fn app_with_replies(
        replies: Vec<(&'static str, Result<Value, &'static str>)>,
    ) -> (App, Arc<Mutex<Vec<Value>>>) {
        let (incoming, incoming_rx) = mpsc::unbounded_channel();
        let (sent, mut sent_rx) = mpsc::unbounded_channel();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        let mut replies = VecDeque::from(replies);
        tokio::spawn(async move {
            while let Some(frame) = sent_rx.recv().await {
                let request: Value = match frame {
                    MessageFrame::Value(value) => value,
                    MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON request"),
                };
                let method = request["method"].as_str().unwrap_or_default();
                let response = if method.starts_with("session.goal.") {
                    let (expected, result) = replies.pop_front().expect("expected goal request");
                    assert_eq!(method, expected);
                    result
                } else if method == "session.list" {
                    Ok(json!({"sessions": []}))
                } else {
                    Err("unavailable in test")
                };
                sink.lock().expect("requests").push(request.clone());
                let mut reply = json!({"jsonrpc": "2.0", "id": request["id"]});
                match response {
                    Ok(result) => reply["result"] = result,
                    Err(message) => reply["error"] = json!({"code": -32000, "message": message}),
                }
                if incoming.send(MessageFrame::Value(reply)).is_err() {
                    break;
                }
            }
        });
        let client = Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        });
        let mut app = App::new_with_config(client, false, TuiConfig::default(), Theme::default())
            .await
            .expect("app");
        install_draft_catalog(&mut app);
        recorded.lock().expect("requests").clear();
        (app, recorded)
    }

    fn draft_selection(
        model: &str,
        variant: Option<&str>,
        preset: Option<&str>,
    ) -> cookie_agent_protocol::RunSelection {
        serde_json::from_value(json!({
            "agent": "primary", "model": { "model": model, "variant": variant }, "preset": preset,
        }))
        .unwrap()
    }

    fn install_draft_catalog(app: &mut App) {
        use cookie_agent_protocol::{AgentDescriptor, AgentMode, Sha256Digest};

        let selection = draft_selection("test/model", None, None);
        app.agents = [None, Some("review".to_owned())]
            .into_iter()
            .map(|preset| AgentDescriptor {
                id: selection.agent.clone(),
                preset,
                description: "Primary test agent".into(),
                mode: AgentMode::Primary,
                enabled: true,
                runnable_as_root: true,
                resolved_fallback: vec![selection.model.clone()],
                delegation_targets: Vec::new(),
            })
            .collect();
        app.models = ["test/model", "test/model-b"]
            .into_iter()
            .map(|model| {
                serde_json::from_value(json!({
                    "key": model, "display_name": model,
                    "capabilities": {
                        "input": ["text"], "output": ["text"], "context_tokens": 8192,
                        "output_tokens": 2048, "tool_calling": true, "parallel_tool_calls": true,
                        "structured_output": false, "reasoning": true, "temperature": true,
                        "top_p": true, "seed": true,
                        "native_replay": cookie_agent_protocol::ReplayCapability::Optional,
                        "cancellation": cookie_agent_protocol::CancellationCapability::LocalOnly,
                        "media": {},
                    },
                    "variants": [{
                        "id": "high", "display_name": "High",
                        "origin": cookie_agent_protocol::VariantOrigin::Explicit,
                        "behavior_fingerprint": Sha256Digest::of_bytes(b"high"),
                    }],
                    "variant_order": ["high"], "default_variant": "high",
                    "behavior_fingerprint": Sha256Digest::of_bytes(model.as_bytes()),
                }))
                .unwrap()
            })
            .collect();
        app.draft = Some(selection);
    }

    fn goal(status: GoalStatus) -> GoalState {
        GoalState {
            goal_id: GoalId::new_v7(),
            objective: "finish  the parser".into(),
            status,
            items: vec![
                GoalItem {
                    description: "Parse commands".into(),
                    finished: true,
                },
                GoalItem {
                    description: "Verify behavior".into(),
                    finished: false,
                },
            ],
            revision: 7,
        }
    }

    fn meta(session_id: SessionId, origin: SessionOrigin) -> SessionMeta {
        let revision = format!("sha256:{}", "1".repeat(64));
        serde_json::from_value(json!({
            "session_id": session_id,
            "origin": origin,
            "cwd_identity": "/workspace",
            "creation_selection": {"agent": "primary", "model": {"model": "test/model", "variant": null}, "preset": null},
            "runtime_revision": revision, "catalog_revision": revision, "provider_state_revision": revision,
            "model_revision": revision, "agent_revision": revision, "recipe_registry_revision": revision, "manifest_revision": revision,
            "title": null, "title_updated_seq": 0, "last_event_seq": 1,
            "last_activity": "2026-08-06T12:00:00Z", "status": "idle", "skipped_events": []
        })).expect("session metadata")
    }

    async fn dispatch(app: &mut App, input: &str) {
        let Submission::Command(command) = parse_submission(input).expect("command") else {
            panic!("expected command");
        };
        tokio::time::timeout(Duration::from_secs(1), app.run_command(command))
            .await
            .expect("dispatch does not wait for RPC");
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("goal response")
            .expect("update");
        assert!(matches!(update, RpcUpdate::GoalFinished { .. }));
        app.handle_rpc_update(update);
    }

    async fn finish_rpc(app: &mut App) {
        let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
            .await
            .expect("goal response")
            .expect("update");
        assert!(matches!(update, RpcUpdate::GoalFinished { .. }));
        app.handle_rpc_update(update);
    }

    fn mount_goal(app: &mut App, session_id: SessionId, goal: GoalState) {
        app.selected = Some(session_id);
        app.sessions.push(meta(session_id, SessionOrigin::Root));
        app.store.sessions.insert(
            session_id,
            SessionState {
                goal: Some(goal),
                ..Default::default()
            },
        );
    }

    #[tokio::test]
    async fn goal_activation_uses_exact_set_request_without_starting_a_run() {
        let expected = goal(GoalStatus::Active);
        let (mut app, requests) =
            app_with_replies(vec![("session.goal.set", Ok(json!({"goal": expected})))]).await;
        let session_id = SessionId::new_v7();
        app.selected = Some(session_id);
        app.sessions.push(meta(session_id, SessionOrigin::Root));
        let selection = app.draft.clone().unwrap();
        dispatch(&mut app, "/goal finish  the parser").await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "session.goal.set");
        assert_eq!(
            requests[0]["params"],
            json!({"session_id": session_id, "objective": "finish  the parser", "selection": selection})
        );
        assert!(!app.goal_notices.contains_key(&session_id));
    }

    #[tokio::test]
    async fn lifecycle_buttons_fetch_fresh_identity_and_revision() {
        use ratatui::{Terminal, backend::TestBackend};

        for (projected_status, button, action, changed_status) in [
            (
                GoalStatus::Active,
                GoalBarAction::Pause,
                "pause",
                GoalStatus::Paused,
            ),
            (
                GoalStatus::Paused,
                GoalBarAction::Resume,
                "resume",
                GoalStatus::Active,
            ),
            (
                GoalStatus::Active,
                GoalBarAction::Cancel,
                "cancel",
                GoalStatus::Cancelled,
            ),
        ] {
            let current = goal(projected_status);
            let changed = GoalState {
                status: changed_status,
                revision: 8,
                ..current.clone()
            };
            let (mut app, requests) = app_with_replies(vec![
                ("session.goal.get", Ok(json!({"goal": current}))),
                ("session.goal.lifecycle", Ok(json!({"goal": changed}))),
            ])
            .await;
            let session_id = SessionId::new_v7();
            let mut projected = goal(projected_status);
            projected.revision = 2;
            mount_goal(&mut app, session_id, projected);
            let selection = app.draft.clone().unwrap();
            let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    app.render_goal_bar(frame, area);
                })
                .unwrap();
            let (rect, _) = app
                .hit_map
                .goal_actions
                .iter()
                .find(|(_, action)| *action == button)
                .copied()
                .expect("lifecycle button");
            assert_eq!(
                app.hover_target_at(rect.x, rect.y),
                Some(super::super::HoverTarget::GoalAction(button))
            );
            app.handle_click(rect.x, rect.y).await;
            finish_rpc(&mut app).await;
            assert_eq!(app.status, format!("goal {}", status_name(changed_status)));
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["method"], "session.goal.get");
            assert_eq!(requests[1]["method"], "session.goal.lifecycle");
            let mut params = json!({
                "session_id": session_id, "goal_id": current.goal_id,
                "expected_revision": 7, "action": action,
            });
            if action == "resume" {
                params["selection"] = json!(selection);
            }
            assert_eq!(requests[1]["params"], params);
        }
    }

    #[tokio::test]
    async fn activation_and_resume_send_current_draft_without_changing_running_attribution() {
        use crate::state::{FrozenAssistantAttribution, TranscriptItem};
        use cookie_agent_protocol::{AdaptorId, ResolvedModelRef, RunId, Sha256Digest};

        for command in ["/goal Use the selected model", "/goal resume"] {
            let current_goal = goal(GoalStatus::Paused);
            let replies = if command == "/goal resume" {
                vec![
                    ("session.goal.get", Ok(json!({"goal": current_goal}))),
                    ("session.goal.lifecycle", Ok(json!({"goal": current_goal}))),
                ]
            } else {
                vec![("session.goal.set", Ok(json!({"goal": current_goal})))]
            };
            let (mut app, requests) = app_with_replies(replies).await;
            let session_id = SessionId::new_v7();
            mount_goal(&mut app, session_id, current_goal);
            let previous = app.sessions[0].creation_selection.clone();
            let selected = draft_selection("test/model-b", Some("high"), Some("review"));
            app.draft = Some(selected.clone());
            let run = RunId::new_v7();
            let attribution = FrozenAssistantAttribution {
                agent: previous.agent.clone(),
                resolved_model: ResolvedModelRef {
                    selection: previous.model.clone(),
                    provider_id: previous.model.model.provider_id(),
                    model_id: previous.model.model.model_id(),
                    adapter_id: AdaptorId::OpenaiCompatible,
                    selection_fingerprint: Sha256Digest::of_bytes(b"running-model-a"),
                },
            };
            let original_header = attribution.header();
            let state = app.store.sessions.get_mut(&session_id).unwrap();
            if command != "/goal resume" {
                state.goal = None;
            }
            state.active_run = Some(run);
            state.run_agent = Some(previous.agent.clone());
            state.transcript.push(TranscriptItem::Assistant {
                id: 1,
                version: 0,
                attribution,
                committed_turn_seq: Some(1),
                children: Vec::new(),
            });
            let title_before = app.message_title_spans();
            dispatch(&mut app, command).await;
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.last().unwrap()["params"]["selection"],
                json!(selected)
            );
            assert!(requests.iter().all(
                |request| request["method"] != "run.start" && request["method"] != "run.steer"
            ));
            assert_eq!(app.draft.as_ref(), Some(&selected));
            assert_eq!(app.message_title_spans(), title_before);
            assert_eq!(app.sessions[0].creation_selection, previous);
            let state = &app.store.sessions[&session_id];
            assert_eq!(state.active_run, Some(run));
            let TranscriptItem::Assistant { attribution, .. } = &state.transcript[0] else {
                panic!("assistant")
            };
            assert_eq!(attribution.header(), original_header);
        }
    }

    #[tokio::test]
    async fn goal_selection_uses_normal_draft_validation_and_pause_cancel_do_not_normalize() {
        let current_goal = goal(GoalStatus::Paused);
        let (mut app, _) = app_with_replies(vec![(
            "session.goal.set",
            Ok(json!({"goal": current_goal})),
        )])
        .await;
        let session_id = SessionId::new_v7();
        mount_goal(&mut app, session_id, current_goal.clone());
        let invalid = draft_selection("test/model-b", Some("removed-variant"), Some("review"));
        app.draft = Some(invalid.clone());
        let normalized = app
            .validated_draft_selection()
            .expect("normal submission draft");
        assert_eq!(normalized.model.variant.as_ref().unwrap().as_str(), "high");
        app.draft = Some(invalid.clone());
        dispatch(&mut app, "/goal Normalize the selected draft").await;
        assert_eq!(app.draft, Some(normalized));

        for command in ["/goal pause", "/goal cancel"] {
            let (mut app, requests) = app_with_replies(vec![
                ("session.goal.get", Ok(json!({"goal": current_goal}))),
                ("session.goal.lifecycle", Ok(json!({"goal": current_goal}))),
            ])
            .await;
            mount_goal(&mut app, session_id, current_goal.clone());
            app.draft = Some(invalid.clone());
            dispatch(&mut app, command).await;
            assert_eq!(app.draft.as_ref(), Some(&invalid));
            assert!(
                requests.lock().unwrap().last().unwrap()["params"]
                    .get("selection")
                    .is_none()
            );
        }
        let (mut app, requests) = app_with_replies(Vec::new()).await;
        mount_goal(&mut app, session_id, current_goal);
        app.draft = None;
        for command in [
            GoalCommand::Resume,
            GoalCommand::Objective("Need a model".into()),
        ] {
            app.run_goal_command(command);
            assert!(app.status.contains("select a draft agent/model"));
        }
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn goal_errors_are_visible_and_do_not_mutate_projection_or_composer() {
        for (command, replies, error) in [
            (
                "/goal pause",
                vec![("session.goal.get", Ok(json!({"goal": null})))],
                "no goal is set",
            ),
            (
                "/goal new objective",
                vec![("session.goal.set", Err("goal is already active"))],
                "goal is already active",
            ),
            (
                "/goal cancel",
                vec![
                    (
                        "session.goal.get",
                        Ok(json!({"goal": goal(GoalStatus::Active)})),
                    ),
                    ("session.goal.lifecycle", Err("stale goal revision")),
                ],
                "stale goal revision",
            ),
        ] {
            let (mut app, _) = app_with_replies(replies).await;
            let session_id = SessionId::new_v7();
            let current = goal(GoalStatus::Paused);
            app.selected = Some(session_id);
            app.store.sessions.insert(
                session_id,
                SessionState {
                    goal: Some(current.clone()),
                    ..Default::default()
                },
            );
            app.input.set_buffer("user draft".into());
            dispatch(&mut app, command).await;
            assert!(app.status.contains(error), "{}", app.status);
            assert!(
                app.goal_notices[&session_id]
                    .last()
                    .unwrap()
                    .contains(error)
            );
            assert_eq!(app.store.sessions[&session_id].goal, Some(current));
            assert_eq!(app.input.as_str(), "user draft");
        }
    }

    #[tokio::test]
    async fn goal_guards_reject_missing_child_read_only_and_empty_activation() {
        let (mut app, requests) = app_with_replies(Vec::new()).await;
        app.run_goal_command(GoalCommand::Pause);
        assert!(app.status.contains("select a root session"));
        let session_id = SessionId::new_v7();
        app.selected = Some(session_id);
        app.sessions.push(meta(
            session_id,
            SessionOrigin::Delegated {
                root_session_id: SessionId::new_v7(),
                parent_session_id: SessionId::new_v7(),
                parent_run_id: cookie_agent_protocol::RunId::new_v7(),
                parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
                invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                depth: 1,
            },
        ));
        app.store.sessions.insert(
            session_id,
            SessionState {
                goal: Some(goal(GoalStatus::Active)),
                ..Default::default()
            },
        );
        assert!(!app.goal_bar_visible());
        app.open_goal_detail();
        assert_eq!(app.modal, Modal::None);
        for action in [
            GoalBarAction::Pause,
            GoalBarAction::Resume,
            GoalBarAction::Cancel,
        ] {
            app.activate_goal_action(action);
            assert!(app.status.contains("only available in root sessions"));
        }
        for command in [
            GoalCommand::Pause,
            GoalCommand::Resume,
            GoalCommand::Cancel,
            GoalCommand::Objective("test".into()),
        ] {
            app.run_goal_command(command);
            assert!(app.status.contains("only available in root sessions"));
        }
        app.sessions[0].origin = SessionOrigin::Root;
        app.read_only_sessions.insert(session_id);
        app.run_goal_command(GoalCommand::Pause);
        assert!(app.status.contains("read-only"));
        app.read_only_sessions.clear();
        app.run_goal_command(GoalCommand::Objective(" \t ".into()));
        assert!(app.status.contains("must not be empty"));
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn goal_responses_stay_with_their_session_and_do_not_replace_newer_events() {
        let (mut app, _) = app_with_replies(Vec::new()).await;
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        let current = goal(GoalStatus::Completed);
        app.store.sessions.insert(
            first,
            SessionState {
                goal: Some(current.clone()),
                ..Default::default()
            },
        );
        app.selected = Some(second);
        app.status = "second session".into();
        app.finish_goal_command(first, Ok(Some(goal(GoalStatus::Active))));
        assert_eq!(app.status, "second session");
        assert!(!app.goal_notices.contains_key(&second));
        assert_eq!(app.store.sessions[&first].goal, Some(current));
    }

    #[tokio::test]
    async fn goal_bar_is_bounded_and_has_non_overlapping_hits_at_tiny_widths() {
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        let (mut app, _) = app_with_replies(Vec::new()).await;
        let session_id = SessionId::new_v7();
        let mut current = goal(GoalStatus::Paused);
        current.objective = "Long objective\nwith\ttabs and a long sequence of work ".repeat(10);
        mount_goal(&mut app, session_id, current);
        for width in [1, 3, 7, 8, 20, 40, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| app.render_goal_bar(frame, Rect::new(0, 0, width, 1)))
                .unwrap();
            let line = (0..width)
                .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                .collect::<String>();
            assert!(UnicodeWidthStr::width(line.as_str()) <= usize::from(width));
            assert!(!line.contains(['\n', '\t']));
            if width >= 20 {
                assert!(line.starts_with("Long objective"), "{line}");
            }
            assert!(
                app.hit_map
                    .goal_actions
                    .iter()
                    .any(|(_, action)| *action == GoalBarAction::Details)
            );
            for (index, (rect, _)) in app.hit_map.goal_actions.iter().enumerate() {
                assert!(rect.right() <= width, "{width}: {rect:?}");
                for (other, _) in app.hit_map.goal_actions.iter().skip(index + 1) {
                    assert!(
                        rect.intersection(*other).is_empty(),
                        "{width}: {rect:?} {other:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn terminal_and_read_only_goals_expose_no_lifecycle_buttons() {
        let (mut app, _) = app_with_replies(Vec::new()).await;
        let session_id = SessionId::new_v7();
        mount_goal(&mut app, session_id, goal(GoalStatus::Completed));
        assert_eq!(app.allowed_goal_actions(), vec![GoalBarAction::Details]);
        app.cycle_goal_focus(false);
        assert_eq!(app.goal_focus, Some(GoalBarAction::Details));
        app.store.sessions.get_mut(&session_id).unwrap().goal = Some(goal(GoalStatus::Active));
        app.read_only_sessions.insert(session_id);
        assert_eq!(app.allowed_goal_actions(), vec![GoalBarAction::Details]);
        app.cycle_goal_focus(false);
        assert_eq!(app.goal_focus, Some(GoalBarAction::Details));
        app.activate_goal_action(GoalBarAction::Pause);
        assert!(app.status.contains("read-only"));
        app.read_only_sessions.clear();
        assert_eq!(
            app.allowed_goal_actions(),
            vec![
                GoalBarAction::Details,
                GoalBarAction::Pause,
                GoalBarAction::Cancel
            ]
        );
        app.store.sessions.get_mut(&session_id).unwrap().goal = Some(goal(GoalStatus::Paused));
        assert_eq!(
            app.allowed_goal_actions(),
            vec![
                GoalBarAction::Details,
                GoalBarAction::Resume,
                GoalBarAction::Cancel
            ]
        );
    }

    #[tokio::test]
    async fn detail_modal_opens_scrolls_and_closes_when_session_changes() {
        use crossterm::event::KeyModifiers;
        use ratatui::{Terminal, backend::TestBackend};

        let (mut app, _) = app_with_replies(Vec::new()).await;
        let session_id = SessionId::new_v7();
        let mut current = goal(GoalStatus::Active);
        current.items = (0..20)
            .map(|index| GoalItem {
                description: format!("ordered checklist item {index}"),
                finished: index % 2 == 0,
            })
            .collect();
        mount_goal(&mut app, session_id, current);
        app.open_goal_detail();
        assert_eq!(app.modal, Modal::GoalDetail);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| app.render_goal_detail(frame))
            .unwrap();
        assert!(app.goal_detail.max_scroll > 0);
        app.handle_goal_detail_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(app.goal_detail.scroll > 0);
        app.handle_goal_detail_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.goal_detail.scroll, 0);
        for width in [1, 3, 7, 8, 20, 40, 80] {
            let mut narrow = Terminal::new(TestBackend::new(width, 10)).unwrap();
            narrow.draw(|frame| app.render_goal_detail(frame)).unwrap();
            if let Some(close) = app.hit_map.goal_close {
                assert!(close.right() <= width);
                assert!(close.bottom() <= 10);
            }
            app.handle_goal_detail_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            app.handle_goal_detail_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(app.goal_detail.scroll, app.goal_detail.max_scroll);
            app.handle_goal_detail_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        }

        app.handle_goal_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.modal, Modal::None);
        app.open_goal_detail();

        app.selected = Some(SessionId::new_v7());
        terminal
            .draw(|frame| app.render_goal_detail(frame))
            .unwrap();
        assert_eq!(app.modal, Modal::None);
        assert_eq!(app.goal_detail.session_id, None);
    }

    #[tokio::test]
    async fn producer_reminders_never_restore_into_the_user_composer() {
        use cookie_agent_protocol::{
            EventPayload, GoalReminderIdentity, ProducerDeliveryMode, ProducerIdempotencyKey,
            ProducerMessageId, ProducerOwner, RunId, StoredEvent,
        };

        let (mut app, _) = app_with_replies(Vec::new()).await;
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let goal_id = GoalId::new_v7();
        app.selected = Some(session_id);
        for (index, payload) in [
            EventPayload::ProducerMessageAccepted {
                message_id: ProducerMessageId::new_v7(),
                producer_owner: ProducerOwner::Goal { goal_id },
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("reminder").unwrap(),
                body: "INTERNAL REMINDER".into(),
                reminder: Some(GoalReminderIdentity {
                    goal_id,
                    revision: 1,
                }),
            },
            EventPayload::UserInputAdmitted {
                input: "user pending text".into(),
            },
            EventPayload::RunInterrupted { reason: None },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(app.store.apply_event(StoredEvent {
                engine_version: None,
                origin: None,
                session_id,
                run_id: Some(run_id),
                seq: index as u64 + 1,
                timestamp: "2026-08-06T12:00:00Z".parse().unwrap(),
                payload,
            }));
        }
        app.restore_voided_inputs();
        assert_eq!(app.input.as_str(), "user pending text");
        assert!(app.store.sessions[&session_id].voided_inputs.is_empty());
    }
}
