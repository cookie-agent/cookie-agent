//! What the TUI tells the daemon it shows, so the daemon pushes no
//! notification for it (`docs/site/specs/push-notifications.md` section
//! 8): the viewed session while the terminal has focus and was used in the
//! last two minutes. A terminal that never reports focus never says the
//! user is there, so its user still gets pushes.

use std::time::Instant;

use cookie_agent_client_state::presence::{daemon_has_push, shown};
use cookie_agent_protocol::SessionId;

use super::App;

#[derive(Debug)]
pub(in crate::ui) struct Presence {
    /// Whether the terminal has focus; `None` until it first reports it.
    focused: Option<bool>,
    last_input: Instant,
    /// What this connection was last told; `None` before it was told
    /// anything.
    sent: Option<Option<SessionId>>,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            focused: None,
            last_input: Instant::now(),
            sent: None,
        }
    }
}

impl Presence {
    /// What to tell the daemon the TUI shows at `now` with `viewed` on
    /// screen, recorded as told; `None` when it was told that already.
    fn next(&mut self, viewed: Option<SessionId>, now: Instant) -> Option<Option<SessionId>> {
        let shown = shown(
            viewed,
            self.focused == Some(true),
            now.saturating_duration_since(self.last_input),
        );
        if self.sent == Some(shown) {
            return None;
        }
        self.sent = Some(shown);
        Some(shown)
    }
}

impl App {
    /// The terminal gained or lost focus.
    pub(in crate::ui) fn focus_changed(&mut self, focused: bool) {
        self.presence.focused = Some(focused);
        if focused {
            self.presence.last_input = Instant::now();
        }
        self.report_presence();
    }

    /// A key, a click, or a paste: the user is there.
    pub(in crate::ui) fn note_input(&mut self) {
        self.presence.last_input = Instant::now();
        self.report_presence();
    }

    /// A new connection has been told nothing yet.
    pub(in crate::ui) fn reset_presence(&mut self) {
        self.presence.sent = None;
    }

    /// Tells the daemon what the TUI shows now, when that changed.
    pub(in crate::ui) fn report_presence(&mut self) {
        if !daemon_has_push(self.client.daemon_version()) {
            return;
        }
        let Some(now) = self.presence.next(self.viewed_session(), Instant::now()) else {
            return;
        };
        let client = self.client.clone();
        self.spawn_rpc(async move {
            let _ = client.set_presence(now).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cookie_agent_client_state::presence::IDLE_AFTER;

    use super::*;

    #[test]
    fn the_daemon_hears_a_focused_session_once_and_its_end() {
        let session = SessionId::new_v7();
        let start = Instant::now();
        let mut presence = Presence {
            last_input: start,
            ..Presence::default()
        };
        // A terminal that never reported focus says the user is not there.
        assert_eq!(presence.next(Some(session), start), Some(None));
        assert_eq!(presence.next(Some(session), start), None);
        presence.focused = Some(true);
        assert_eq!(presence.next(Some(session), start), Some(Some(session)));
        assert_eq!(presence.next(Some(session), start), None);
        // Idle, then focus lost: told once each time it changes.
        assert_eq!(presence.next(Some(session), start + IDLE_AFTER), Some(None));
        presence.last_input = start + IDLE_AFTER;
        assert_eq!(
            presence.next(Some(session), start + IDLE_AFTER + Duration::from_secs(1)),
            Some(Some(session))
        );
        presence.focused = Some(false);
        assert_eq!(presence.next(Some(session), start + IDLE_AFTER), Some(None));
        // A new connection is told again.
        presence.sent = None;
        assert_eq!(presence.next(None, start), Some(None));
    }
}
