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
        let prefix = if area.width >= 12 && details_width >= 8 {
            "🎯: "
        } else {
            ""
        };
        let mut summary = single_line(&goal.objective);
        if matches!(goal.status, GoalStatus::Completed | GoalStatus::Cancelled) {
            summary.push_str(&format!(" · {}", status_name(goal.status)));
        }
        let objective_width = details_width.saturating_sub(UnicodeWidthStr::width(prefix));
        let mut details = truncate_with_ellipsis(&summary, objective_width);
        details.push_str(
            &" ".repeat(objective_width.saturating_sub(UnicodeWidthStr::width(details.as_str()))),
        );
        let details_style = if self.goal_focus == Some(GoalBarAction::Details) {
            self.theme.assistant().patch(self.theme.block_hover())
        } else {
            self.theme.assistant()
        };
        let mut spans = vec![
            Span::styled(prefix, self.theme.internal()),
            Span::styled(details, details_style),
        ];
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
#[path = "goal/tests.rs"]
mod tests;
