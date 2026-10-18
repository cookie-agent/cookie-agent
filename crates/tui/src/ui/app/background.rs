//! Background calls (`docs/site/specs/background-tools.md` section 9): the
//! title-row button that moves a running call to the background, the bar
//! above the composer while the session has background calls, and the panel
//! that lists them and stops one.

use cookie_agent_protocol::{BackgroundActor, BackgroundCall, ToolCallId};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::{App, Modal, RpcUpdate, paint_panel, truncate_with_ellipsis};

/// The bar's control that opens the panel.
const DETAILS: &str = " [Details]";
/// Each panel row's control that stops its call.
const STOP: &str = " [stop]";

#[derive(Debug, Default)]
pub(super) struct BackgroundPanelState {
    pub(super) selected: usize,
}

impl App {
    /// The selected session's background calls.
    pub(super) fn background_calls(&self) -> &[BackgroundCall] {
        self.selected
            .and_then(|session_id| self.store.sessions.get(&session_id))
            .map_or(&[], |state| state.background_calls.as_slice())
    }

    pub(super) fn background_bar_visible(&self) -> bool {
        !self.background_calls().is_empty()
    }

    /// Asks the daemon to move the running call `call_id` to the background.
    pub(super) fn request_background(&mut self, call_id: ToolCallId) {
        let Some(run_id) = self.selected_active_run() else {
            self.status = "the call is no longer running".into();
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.status = "moving the call to the background…".into();
        self.spawn_rpc(async move {
            let update = match client
                .background_tool(cookie_agent_protocol::RunToolBackgroundParams { run_id, call_id })
                .await
            {
                Ok(result) if result.accepted => {
                    RpcUpdate::Notice("the call moves to the background once it can".into())
                }
                Ok(_) => RpcUpdate::Notice("this call cannot move to the background".into()),
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }

    pub(super) fn render_background_bar(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.hit_map.background_bar = None;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let calls = self.background_calls();
        if calls.is_empty() {
            return;
        }
        let summary = calls
            .iter()
            .map(|call| match &call.display {
                Some(display) => format!("{}: {}", call.tool, single_line(display)),
                None => call.tool.clone(),
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let width = usize::from(area.width);
        let controls = if width >= UnicodeWidthStr::width(DETAILS) + 16 {
            DETAILS
        } else {
            ""
        };
        let details_width = width.saturating_sub(UnicodeWidthStr::width(controls));
        let mut text = truncate_with_ellipsis(
            &format!("⏳ {} in background: {summary}", calls.len()),
            details_width,
        );
        text.push_str(
            &" ".repeat(details_width.saturating_sub(UnicodeWidthStr::width(text.as_str()))),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(text, self.theme.assistant()),
                Span::styled(controls, self.theme.link()),
            ]))
            .style(self.theme.panel()),
            area,
        );
        self.hit_map.background_bar = Some(area);
    }

    pub(super) fn open_background_panel(&mut self) {
        if self.background_calls().is_empty() {
            self.status = "no calls are running in the background".into();
            return;
        }
        self.background_panel = BackgroundPanelState::default();
        self.modal = Modal::Background;
    }

    pub(super) fn render_background_panel(&mut self, frame: &mut ratatui::Frame) {
        self.hit_map.background_rows.clear();
        self.hit_map.background_stops.clear();
        let calls = self.background_calls().to_vec();
        if calls.is_empty() {
            self.modal = Modal::None;
            return;
        }
        self.background_panel.selected = self.background_panel.selected.min(calls.len() - 1);
        let frame_area = frame.area();
        if frame_area.width < 8 || frame_area.height < 4 {
            return;
        }
        let width = frame_area.width.min(90);
        let height = frame_area.height.min(
            u16::try_from(calls.len())
                .unwrap_or(u16::MAX)
                .saturating_add(2),
        );
        let area = Rect::new(
            frame_area.x + (frame_area.width - width) / 2,
            frame_area.y + (frame_area.height - height) / 2,
            width,
            height,
        );
        paint_panel(frame, area, &self.theme);
        let inner_width = usize::from(area.width.saturating_sub(2));
        let row_width = inner_width.saturating_sub(UnicodeWidthStr::width(STOP));
        let now = jiff::Timestamp::now();
        let lines = calls
            .iter()
            .enumerate()
            .take(usize::from(area.height.saturating_sub(2)))
            .map(|(index, call)| {
                let elapsed = now.duration_since(call.started_at).as_secs().max(0);
                let by = match call.by {
                    BackgroundActor::Model => "by the model",
                    BackgroundActor::User => "by you",
                };
                let row = format!(
                    "{} {}{} · {} · {by}",
                    call.job,
                    call.tool,
                    call.display
                        .as_deref()
                        .map(|display| format!(": {}", single_line(display)))
                        .unwrap_or_default(),
                    elapsed_label(elapsed),
                );
                let mut row = truncate_with_ellipsis(&row, row_width);
                row.push_str(
                    &" ".repeat(row_width.saturating_sub(UnicodeWidthStr::width(row.as_str()))),
                );
                let style = if index == self.background_panel.selected {
                    self.theme.selected()
                } else {
                    self.theme.body()
                };
                let y = area.y + 1 + u16::try_from(index).unwrap_or(u16::MAX);
                self.hit_map
                    .background_rows
                    .push((Rect::new(area.x + 1, y, row_width as u16, 1), index));
                self.hit_map.background_stops.push((
                    Rect::new(
                        area.x + 1 + row_width as u16,
                        y,
                        UnicodeWidthStr::width(STOP) as u16,
                        1,
                    ),
                    index,
                ));
                Line::from(vec![
                    Span::styled(row, style),
                    Span::styled(STOP, self.theme.link()),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(
                crate::ui::panel_block()
                    .border_style(self.theme.panel_border())
                    .title(crate::ui::panel_title(Span::styled(
                        truncate_with_ellipsis("Background calls", inner_width),
                        self.theme.heading(),
                    )))
                    .title_bottom(crate::ui::panel_title(
                        Line::from(Span::styled(
                            truncate_with_ellipsis("Up/Down · s: stop · Esc: close", inner_width),
                            self.theme.internal(),
                        ))
                        .right_aligned(),
                    )),
            ),
            area,
        );
    }

    pub(super) fn handle_background_key(&mut self, key: KeyEvent) {
        let count = self.background_calls().len();
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.modal = Modal::None,
            KeyCode::Up => {
                self.background_panel.selected = self.background_panel.selected.saturating_sub(1);
            }
            KeyCode::Down if count > 0 => {
                self.background_panel.selected =
                    (self.background_panel.selected + 1).min(count - 1);
            }
            KeyCode::Char('s') | KeyCode::Delete => {
                self.stop_background_call(self.background_panel.selected);
            }
            _ => {}
        }
    }

    /// Stops the background call at `index` of the selected session's list,
    /// as the user. It still reports to the model.
    pub(super) fn stop_background_call(&mut self, index: usize) {
        let (Some(session_id), Some(call)) =
            (self.selected, self.background_calls().get(index).cloned())
        else {
            return;
        };
        self.background_panel.selected = index;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.status = format!("stopping {}…", call.job);
        self.spawn_rpc(async move {
            let update = match client.stop_background(session_id, call.job.clone()).await {
                Ok(_) => RpcUpdate::Notice(format!("stopped {}", call.job)),
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn elapsed_label(seconds: i64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests;
