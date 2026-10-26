//! The held banner (`docs/site/specs/held-sessions.md`): while the viewed
//! session's automatic runs are held, the conversation's last row says why
//! and what waits, with a *resume* button; Enter in an empty composer
//! resumes too, and a message sent resumes as it goes.

use super::*;

const RESUME_BUTTON: &str = " resume ";

impl App {
    /// The banner's text while the viewed session is held.
    pub(super) fn held_banner_text(&self) -> Option<String> {
        let state = self.store.sessions.get(&self.selected?)?;
        cookie_agent_client_state::held::composer_line(state)
    }

    /// The banner row: why the session is held, what waits, and the button.
    pub(super) fn render_held_banner(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.hit_map.held_resume = None;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(text) = self.held_banner_text() else {
            return;
        };
        let width = usize::from(area.width);
        let button = if width >= UnicodeWidthStr::width(RESUME_BUTTON) + 12 {
            RESUME_BUTTON
        } else {
            ""
        };
        let button_width = UnicodeWidthStr::width(button);
        let text_width = width.saturating_sub(button_width + 1);
        let mut text = truncate_with_ellipsis(&format!("⏸ {text}"), text_width);
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
            self.hit_map.held_resume = Some(Rect::new(x, area.y, button_width, 1));
        }
    }

    /// Clears the viewed session's hold (`session.held.resume`).
    pub(super) fn resume_held(&mut self) {
        let Some(session_id) = self.selected else {
            return;
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client
                .resume_held_session(cookie_agent_protocol::SessionHeldResumeParams { session_id })
                .await
            {
                Ok(result) if result.resumed => RpcUpdate::Notice("resumed".into()),
                Ok(_) => RpcUpdate::Notice("the session was not held".into()),
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }
}
