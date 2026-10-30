//! The status message, in the composer's bottom border: it shows for
//! [`STATUS_LIVE`], dims out over the last [`STATUS_FADE`] of it, and is
//! cleared. What stays true for a while (the daemon is reconnecting, the
//! model catalog did not refresh) shows there too, without fading, for as
//! long as it lasts.

use std::time::{Duration, Instant};

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::{App, truncate_with_ellipsis};

/// How long a status message shows, its fade included.
pub(super) const STATUS_LIVE: Duration = Duration::from_secs(5);
/// The end of [`STATUS_LIVE`], over which the message dims out.
const STATUS_FADE: Duration = Duration::from_secs(1);
/// The fade's steps; the 250 ms wall-clock watchdog draws each.
const FADE_STEPS: u8 = 4;
/// Between what lasts and the status message.
const SEPARATOR: &str = " · ";

impl App {
    /// Starts the clock of a new status message; the event loop calls it
    /// before each draw.
    pub(in crate::ui) fn follow_status(&mut self) {
        self.follow_status_at(Instant::now());
    }

    fn follow_status_at(&mut self, now: Instant) {
        if self.status.is_empty() {
            self.status_shown = None;
        } else if self
            .status_shown
            .as_ref()
            .is_none_or(|(shown, _)| *shown != self.status)
        {
            self.status_shown = Some((self.status.clone(), now));
        }
    }

    /// Clears the status message once it has shown for [`STATUS_LIVE`];
    /// whether it did.
    pub(in crate::ui) fn expire_status(&mut self) -> bool {
        self.expire_status_at(Instant::now())
    }

    pub(in crate::ui) fn expire_status_at(&mut self, now: Instant) -> bool {
        self.follow_status_at(now);
        match &self.status_shown {
            Some((_, since)) if now.saturating_duration_since(*since) >= STATUS_LIVE => {
                self.status.clear();
                self.status_shown = None;
                true
            }
            _ => false,
        }
    }

    /// How far the status message has dimmed: 0 until its fade starts, then
    /// up to [`FADE_STEPS`].
    pub(super) fn status_fade(&self) -> u8 {
        self.status_fade_at(Instant::now())
    }

    pub(in crate::ui) fn status_fade_at(&self, now: Instant) -> u8 {
        let Some((shown, since)) = &self.status_shown else {
            return 0;
        };
        if *shown != self.status {
            return 0;
        }
        let fading = now
            .saturating_duration_since(*since)
            .saturating_sub(STATUS_LIVE - STATUS_FADE);
        if fading.is_zero() {
            return 0;
        }
        let step = fading.as_millis() * u128::from(FADE_STEPS) / STATUS_FADE.as_millis() + 1;
        u8::try_from(step).map_or(FADE_STEPS, |step| step.min(FADE_STEPS))
    }

    /// What lasts while it is true, before the status message.
    fn lasting_status(&self) -> Vec<String> {
        let mut lasting = Vec::new();
        if self.reconnecting {
            lasting.push(match &self.reconnect_error {
                Some(error) => format!("daemon disconnected — reconnecting ({error})"),
                None => "daemon disconnected — reconnecting…".to_owned(),
            });
        }
        if let Some(explanation) = self.runtime.durable_explanation()
            && !self.status.contains(explanation)
        {
            lasting.push(explanation.to_owned());
        }
        lasting
    }

    /// Draws the status into the bottom border of the composer at `input`,
    /// right-aligned, in what `taken` columns of attachment chips leave.
    pub(super) fn render_status(&self, frame: &mut ratatui::Frame, input: Rect, taken: u16) {
        if input.height < 2 || input.width < 8 {
            return;
        }
        let mut parts: Vec<(String, Style)> = self
            .lasting_status()
            .into_iter()
            .map(|part| (part, self.theme.muted()))
            .collect();
        if !self.status.is_empty() {
            let fading = self.theme.muted_fading(self.status_fade(), FADE_STEPS + 1);
            parts.push((self.status.clone(), fading));
        }
        // Two border columns each side, and a gap after the chips.
        let room =
            input
                .width
                .saturating_sub(4)
                .saturating_sub(if taken > 0 { taken + 1 } else { 0 });
        // A blank column either side of the text.
        let mut left = usize::from(room).saturating_sub(2);
        if parts.is_empty() || left < 2 {
            return;
        }
        let mut spans = vec![Span::raw(" ")];
        for (index, (text, style)) in parts.into_iter().enumerate() {
            if index > 0 {
                if left <= SEPARATOR.width() {
                    break;
                }
                spans.push(Span::styled(SEPARATOR, style));
                left -= SEPARATOR.width();
            }
            let text = truncate_with_ellipsis(&text, left);
            left = left.saturating_sub(text.width());
            spans.push(Span::styled(text, style));
        }
        spans.push(Span::raw(" "));
        let line = Line::from(spans);
        let width = u16::try_from(line.width())
            .unwrap_or(u16::MAX)
            .min(input.width);
        let x = input.right().saturating_sub(2).saturating_sub(width);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(x, input.bottom() - 1, width, 1),
        );
    }
}
