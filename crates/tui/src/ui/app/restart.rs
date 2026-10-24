//! Restarting the daemon (`docs/site/specs/engine-configuration.md`
//! section 11): the banner while configuration waits for a restart, `/restart`
//! and its confirmation, and the waiting state until the connection closes.

use cookie_agent_client_state::restart::{
    self as shared, RESTART_WHEN_IDLE, RestartOutcome, daemon_can_restart,
};
use cookie_agent_protocol::{ConfigStatus, DaemonRestartingNotification, RestartWhen};

use super::*;

/// A button of the restart confirmation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum RestartChoice {
    WhenIdle,
    Now,
    Cancel,
}

/// What the TUI knows about restarting the daemon.
#[derive(Debug, Default)]
pub(in crate::ui) struct RestartState {
    /// The daemon's configuration status, for the banner.
    pub(in crate::ui) config_status: Option<ConfigStatus>,
    /// A restart the daemon announced, until the connection closes.
    pub(in crate::ui) restarting: Option<DaemonRestartingNotification>,
    /// The running turns the open confirmation counted.
    pub(in crate::ui) running: usize,
    /// The panel the confirmation opened over, which it returns to.
    pub(in crate::ui) return_to: Option<Modal>,
    status_refresh: super::ListingRefresh,
}

/// The banner's button.
const RESTART_BUTTON: &str = " Restart daemon ";

impl App {
    /// Whether the daemon said in its handshake that it predates
    /// `daemon.restart`. Before a handshake the TUI offers it anyway, and an
    /// older daemon refuses the method.
    fn daemon_too_old_to_restart(&self) -> bool {
        self.client
            .daemon_version()
            .is_some_and(|version| !daemon_can_restart(Some(version)))
    }

    /// The banner's text: the waiting state once the daemon announced a
    /// restart, else what the configuration waits for.
    pub(super) fn restart_banner_text(&self) -> Option<String> {
        if let Some(restarting) = &self.restart.restarting {
            return Some(shared::restarting_text(restarting));
        }
        if self.daemon_too_old_to_restart() {
            return None;
        }
        self.restart
            .config_status
            .as_ref()
            .and_then(shared::restart_banner)
    }

    /// Fetches the configuration status for the banner; a configuration
    /// change announced while it is on its way fetches it again once it
    /// lands.
    pub(in crate::ui) fn poll_config_status(&mut self) {
        if self.daemon_too_old_to_restart() || !self.restart.status_refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let status = client.get_config().await.ok().map(|config| config.status);
            let _ = updates.send(RpcUpdate::ConfigStatusFetched(status));
        });
    }

    pub(super) fn install_config_status(&mut self, status: Option<ConfigStatus>) {
        let again = self.restart.status_refresh.landed();
        if status.is_some() {
            self.restart.config_status = status;
        }
        if again {
            self.poll_config_status();
        }
    }

    /// The daemon announced or updated a restart.
    pub(super) fn note_daemon_restarting(&mut self, restarting: DaemonRestartingNotification) {
        self.status = shared::restarting_text(&restarting);
        self.restart.restarting = Some(restarting);
    }

    /// A new connection: the restart, if any, is over.
    pub(super) fn reset_restart_after_reconnect(&mut self) {
        self.restart.restarting = None;
        self.restart.config_status = None;
    }

    /// `/restart` or the banner's button: asks how to restart.
    pub(in crate::ui) fn open_restart_confirm(&mut self) {
        if self.daemon_too_old_to_restart() {
            self.status = shared::DAEMON_TOO_OLD_TO_RESTART.into();
            return;
        }
        self.restart.running = shared::running_turns(&self.sessions);
        self.restart.return_to = (self.modal != Modal::None).then_some(self.modal);
        self.modal = Modal::RestartConfirm;
    }

    pub(super) fn handle_restart_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.answer_restart_confirm(RestartChoice::Cancel),
            KeyCode::Enter | KeyCode::Char('i' | 'I') => {
                self.answer_restart_confirm(RestartChoice::WhenIdle);
            }
            KeyCode::Char('n' | 'N') => self.answer_restart_confirm(RestartChoice::Now),
            _ => {}
        }
    }

    /// Sends the chosen restart, or backs out.
    pub(in crate::ui) fn answer_restart_confirm(&mut self, choice: RestartChoice) {
        self.modal = self.restart.return_to.take().unwrap_or(Modal::None);
        let when = match choice {
            RestartChoice::Cancel => {
                self.status = "The daemon keeps running.".into();
                return;
            }
            RestartChoice::WhenIdle => RestartWhen::Idle,
            RestartChoice::Now => RestartWhen::Now,
        };
        self.status = "Asking the daemon to restart…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let outcome = RestartOutcome::from_result(when, client.restart_daemon(when).await);
            let _ = updates.send(RpcUpdate::RestartAnswered(outcome));
        });
    }

    pub(super) fn finish_restart_request(&mut self, outcome: RestartOutcome) {
        self.status = outcome.status();
    }

    /// The banner row: its text, and the button while nothing is under way.
    pub(super) fn render_restart_banner(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.hit_map.restart_banner = None;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(text) = self.restart_banner_text() else {
            return;
        };
        let width = usize::from(area.width);
        let button = if self.restart.restarting.is_none()
            && width >= UnicodeWidthStr::width(RESTART_BUTTON) + 12
        {
            RESTART_BUTTON
        } else {
            ""
        };
        let button_width = UnicodeWidthStr::width(button);
        let text_width = width.saturating_sub(button_width + 1);
        let mut text = truncate_with_ellipsis(&format!("⟳ {text}"), text_width);
        text.push_str(
            &" ".repeat(text_width.saturating_sub(UnicodeWidthStr::width(text.as_str()))),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(text, self.theme.assistant()),
                Span::raw(" "),
                Span::styled(button, self.theme.link()),
            ]))
            .style(self.theme.panel()),
            area,
        );
        if !button.is_empty() {
            let x = area
                .x
                .saturating_add(u16::try_from(text_width + 1).unwrap_or(u16::MAX));
            let button_width = u16::try_from(button_width).unwrap_or(u16::MAX);
            self.hit_map.restart_banner = Some(Rect::new(x, area.y, button_width, 1));
        }
    }

    pub(super) fn render_restart_confirm(&mut self, frame: &mut ratatui::Frame, screen: Rect) {
        let theme = &self.theme;
        let running = self.restart.running;
        let body = |width: u16| {
            let mut lines = vec![
                Line::styled("Restart the daemon?", theme.heading()),
                Line::default(),
            ];
            lines.extend(wrapped_line(
                Line::styled(shared::restart_explanation(running), theme.muted_text()),
                width,
            ));
            if running > 0 {
                lines.push(Line::default());
                lines.extend(wrapped_line(
                    Line::styled(
                        format!("{}.", shared::restart_now_label(running)),
                        theme.body(),
                    ),
                    width,
                ));
            }
            lines
        };
        let buttons = [
            (
                RestartChoice::WhenIdle,
                RESTART_WHEN_IDLE,
                "enter/i",
                DecisionTone::Allow,
            ),
            (RestartChoice::Now, "Restart now", "n", DecisionTone::Deny),
            (
                RestartChoice::Cancel,
                "Cancel",
                "esc",
                DecisionTone::Neutral,
            ),
        ];
        self.hit_map.restart_choices =
            render_choice_panel(frame, screen, "Restart daemon", body, &buttons, theme);
    }
}
