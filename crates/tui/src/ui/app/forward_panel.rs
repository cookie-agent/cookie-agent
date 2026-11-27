//! `/forwards` (`docs/site/specs/forwards.md` section 8.2): the daemon's
//! forwards with their route, status, expiry, and traffic; Open, Delete,
//! Change expiry, Share, and New (a form in the editor). The listing is
//! read on connect and on every `forwards.changed`, so the bottom bar can
//! count the active ones.

use cookie_agent_client_state::{
    forward::{
        ForwardLifetime, active_count, daemon_has_forwards, expiry_text, form_text, forward_title,
        parse_form, status_text, traffic_text,
    },
    memory::message_of,
    shares::{ShareLifetime, link_address},
};
use cookie_agent_protocol::{
    Forward, ForwardKind, ForwardShareCreateParams, ForwardUpdateParams, ForwardsListResult,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::{App, ListingRefresh, Modal, RpcUpdate, paint_panel, truncate_with_ellipsis};

/// The editor's request for a new forward.
#[derive(Clone, Debug)]
pub(in crate::ui) struct ForwardEditRequest {
    pub(in crate::ui) text: String,
}

#[derive(Debug, Default)]
pub(in crate::ui) struct ForwardPanelState {
    pub(super) forwards: Vec<Forward>,
    pub(super) error: Option<String>,
    pub(super) refresh: ListingRefresh,
    pub(super) selected: usize,
    /// The forward whose Delete waits for a second `d`.
    pub(super) confirm_delete: Option<String>,
}

impl App {
    /// Whether the daemon keeps forwards (client protocol 1.109).
    pub(in crate::ui) fn daemon_has_forwards(&self) -> bool {
        daemon_has_forwards(self.client.daemon_version())
    }

    /// How many forwards are active, for the bottom bar.
    pub(in crate::ui) fn active_forwards(&self) -> usize {
        active_count(&self.forward_panel.forwards)
    }

    pub(in crate::ui) fn open_forward_panel(&mut self) {
        if !self.daemon_has_forwards() {
            self.status =
                "This daemon does not keep forwards; it needs client protocol 1.109.".into();
            return;
        }
        self.forward_panel.selected = 0;
        self.forward_panel.confirm_delete = None;
        self.modal = Modal::Forwards;
        self.poll_forwards();
    }

    /// Fetches `forwards.list`.
    pub(in crate::ui) fn poll_forwards(&mut self) {
        if !self.daemon_has_forwards() || !self.forward_panel.refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_forwards(None)
                .await
                .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::ForwardsListed { result });
        });
    }

    pub(in crate::ui) fn install_forward_listing(
        &mut self,
        result: Result<ForwardsListResult, String>,
    ) {
        let again = self.forward_panel.refresh.landed();
        match result {
            Ok(listing) => {
                self.forward_panel.forwards = listing.forwards;
                self.forward_panel.error = None;
            }
            Err(error) => self.forward_panel.error = Some(error),
        }
        if again {
            self.poll_forwards();
        }
    }

    fn selected_forward(&self) -> Option<Forward> {
        self.forward_panel
            .forwards
            .get(self.forward_panel.selected)
            .cloned()
    }

    /// Where the daemon's page is, without its fragment.
    fn page_base(&self) -> Option<String> {
        let page = self.page_url.as_deref()?;
        let page = page.split('#').next().unwrap_or(page);
        Some(if page.ends_with('/') {
            page.to_owned()
        } else {
            format!("{page}/")
        })
    }

    /// Opens a web forward's app in the browser, which reaches it once it
    /// has opened the daemon's web page.
    fn open_forward(&mut self, forward: &Forward) {
        let (ForwardKind::Http, Some(address), Some(page)) =
            (forward.kind, forward.address.as_deref(), self.page_base())
        else {
            self.status = match &forward.address {
                Some(address) if forward.kind != ForwardKind::Http => {
                    format!("{} listens on {address}", forward.handle)
                }
                _ => "This forward has no web address to open.".into(),
            };
            return;
        };
        let link = format!("{page}{address}");
        self.status = match crate::ui::open_link::open(&link) {
            Ok(()) => format!("Opened {} in the browser", forward.handle),
            Err(error) => format!("The browser did not open ({error}): {link}"),
        };
    }

    /// Gives `forward` the next expiry after the one it has, from now.
    fn cycle_forward_expiry(&mut self, forward: &Forward) {
        let now = jiff::Timestamp::now();
        let left = forward
            .expires_at
            .map(|at| at.as_second() - now.as_second());
        // The next choice longer than what is left, then Never, then 1h.
        let next = match left {
            None => ForwardLifetime::Hour,
            Some(left) if left < 3_600 => ForwardLifetime::Day,
            Some(left) if left < 86_400 => ForwardLifetime::Week,
            Some(left) if left < 7 * 86_400 => ForwardLifetime::Never,
            Some(_) => ForwardLifetime::Hour,
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let handle = forward.handle.clone();
        self.spawn_rpc(async move {
            let update = match client
                .update_forward(ForwardUpdateParams {
                    forward: handle.clone(),
                    title: None,
                    expires_in_hours: Some(next.hours()),
                })
                .await
            {
                Ok(_) => RpcUpdate::Notice(match next {
                    ForwardLifetime::Never => format!("{handle} now never ends"),
                    other => format!("{handle} now ends in {}", other.label()),
                }),
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }

    /// Makes a 7-day public link to a web forward and copies it.
    fn share_forward(&mut self, forward: &Forward) {
        if forward.kind != ForwardKind::Http {
            self.status = "Only a web forward has share links.".into();
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let handle = forward.handle.clone();
        let page = self.page_base().unwrap_or_default();
        self.spawn_rpc(async move {
            let update = match client
                .create_forward_share(ForwardShareCreateParams {
                    forward: handle,
                    expiry: ShareLifetime::Week.expiry(jiff::Timestamp::now()),
                    reuse: false,
                })
                .await
            {
                Ok(made) => RpcUpdate::ForwardShared {
                    link: link_address(made.share.url.as_deref(), &made.share.path, &page),
                },
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }

    /// A share link was made: copied, and shown.
    pub(in crate::ui) fn install_forward_share(&mut self, link: String) {
        self.copy_to_clipboard(link.clone());
        self.status = format!("Link copied, for 7 days; anyone with it reaches the app: {link}");
    }

    fn delete_forward(&mut self, handle: String) {
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client.delete_forward(handle.clone()).await {
                Ok(_) => RpcUpdate::Notice(format!("Deleted {handle}.")),
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }

    /// New: the form in the editor, its target on the first machine.
    fn begin_new_forward(&mut self) {
        let machine = self
            .runtime_panel
            .machines
            .as_ref()
            .and_then(|machines| machines.as_ref().ok())
            .and_then(|machines| machines.machines.first())
            .map(|machine| machine.name.as_str().to_owned());
        self.status = "editing a new forward".into();
        self.pending_forward_editor = Some(ForwardEditRequest {
            text: form_text(machine.as_deref()),
        });
    }

    /// What the editor left: made, unless empty or unchanged.
    pub(in crate::ui) fn finish_forward_edit(
        &mut self,
        request: ForwardEditRequest,
        edited: Result<String, String>,
    ) {
        let text = match edited {
            Ok(text) => text,
            Err(error) => {
                self.status = format!("The editor failed: {error}");
                return;
            }
        };
        let filled = text
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'));
        if !filled || text == request.text {
            self.status = "No forward made.".into();
            return;
        }
        let params = match parse_form(&text) {
            Ok(params) => params,
            Err(error) => {
                self.status = format!("No forward made: {error}");
                return;
            }
        };
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let update = match client.create_forward(params).await {
                Ok(made) => RpcUpdate::Notice(format!(
                    "Made {}: {}",
                    made.forward.handle,
                    made.forward.route()
                )),
                Err(error) => RpcUpdate::failure(&error),
            };
            let _ = updates.send(update);
        });
    }

    pub(super) fn render_forward_panel(&mut self, frame: &mut ratatui::Frame) {
        self.hit_map.forward_rows.clear();
        let forwards = self.forward_panel.forwards.clone();
        if !forwards.is_empty() {
            self.forward_panel.selected = self.forward_panel.selected.min(forwards.len() - 1);
        }
        let frame_area = frame.area();
        if frame_area.width < 8 || frame_area.height < 4 {
            return;
        }
        let width = frame_area.width.min(110);
        // Two lines a forward, and one for a note.
        let wanted = u16::try_from(forwards.len().max(1) * 2)
            .unwrap_or(u16::MAX)
            .saturating_add(3);
        let height = frame_area.height.min(wanted);
        let area = Rect::new(
            frame_area.x + (frame_area.width - width) / 2,
            frame_area.y + (frame_area.height - height) / 2,
            width,
            height,
        );
        paint_panel(frame, area, &self.theme);
        let inner_width = usize::from(area.width.saturating_sub(2));
        let rows = usize::from(area.height.saturating_sub(3)) / 2;
        let now = jiff::Timestamp::now();
        let mut lines = Vec::new();
        if let Some(error) = &self.forward_panel.error {
            lines.push(Line::from(Span::styled(
                truncate_with_ellipsis(error, inner_width),
                self.theme.error(),
            )));
        } else if forwards.is_empty() {
            lines.push(Line::from(Span::styled(
                truncate_with_ellipsis(
                    "No forwards. n: make one, or ask an agent to forward a web app or a port.",
                    inner_width,
                ),
                self.theme.muted(),
            )));
        }
        // Keep the selected forward in view.
        let first = self
            .forward_panel
            .selected
            .saturating_sub(rows.saturating_sub(1));
        for (index, forward) in forwards.iter().enumerate().skip(first).take(rows) {
            let selected = index == self.forward_panel.selected;
            let style = if selected {
                self.theme.selected()
            } else {
                self.theme.body()
            };
            let head = format!(
                "{} {}  {}  {}",
                if selected { "›" } else { " " },
                forward.handle,
                forward.kind.as_str(),
                forward_title(forward),
            );
            let mut head = truncate_with_ellipsis(&head, inner_width);
            head.push_str(
                &" ".repeat(inner_width.saturating_sub(UnicodeWidthStr::width(head.as_str()))),
            );
            let detail = format!(
                "    {} · {} · {}{}",
                status_text(forward),
                expiry_text(forward, now),
                traffic_text(forward),
                match (&forward.address, forward.kind) {
                    (Some(address), ForwardKind::Http) => format!(" · /{address}"),
                    (Some(address), _) => format!(" · {address}"),
                    (None, _) => String::new(),
                }
            );
            let y = area.y + 1 + u16::try_from(lines.len()).unwrap_or(u16::MAX);
            self.hit_map.forward_rows.push((
                Rect::new(area.x + 1, y, area.width.saturating_sub(2), 2),
                index,
            ));
            lines.push(Line::from(Span::styled(head, style)));
            lines.push(Line::from(Span::styled(
                truncate_with_ellipsis(&detail, inner_width),
                self.theme.muted(),
            )));
        }
        let hint = if self.forward_panel.confirm_delete.is_some() {
            "d again: delete · any other key: keep".to_owned()
        } else {
            "↑↓ · Enter/o: open · e: expiry · s: share · d: delete · n: new · Esc".to_owned()
        };
        frame.render_widget(
            Paragraph::new(lines).block(
                crate::ui::panel_block()
                    .border_style(self.theme.panel_border())
                    .title(crate::ui::panel_title(Span::styled(
                        truncate_with_ellipsis(
                            &format!("Forwards ({})", forwards.len()),
                            inner_width,
                        ),
                        self.theme.heading(),
                    )))
                    .title_bottom(crate::ui::panel_title(
                        Line::from(Span::styled(
                            truncate_with_ellipsis(&hint, inner_width),
                            self.theme.internal(),
                        ))
                        .right_aligned(),
                    )),
            ),
            area,
        );
    }

    pub(super) fn handle_forward_key(&mut self, key: KeyEvent) {
        let count = self.forward_panel.forwards.len();
        let selected = self.selected_forward();
        if let Some(handle) = self.forward_panel.confirm_delete.take() {
            if key.code == KeyCode::Char('d') || key.code == KeyCode::Delete {
                self.delete_forward(handle);
            } else {
                self.status = "Kept it.".into();
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => {
                self.forward_panel.selected = self.forward_panel.selected.saturating_sub(1);
            }
            KeyCode::Down if count > 0 => {
                self.forward_panel.selected = (self.forward_panel.selected + 1).min(count - 1);
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(forward) = selected {
                    self.open_forward(&forward);
                }
            }
            KeyCode::Char('e') => {
                if let Some(forward) = selected {
                    self.cycle_forward_expiry(&forward);
                }
            }
            KeyCode::Char('s') => {
                if let Some(forward) = selected {
                    self.share_forward(&forward);
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(forward) = selected {
                    self.status = format!(
                        "Delete {}? Its connections close and its links end: d again to delete.",
                        forward_title(&forward)
                    );
                    self.forward_panel.confirm_delete = Some(forward.handle);
                }
            }
            KeyCode::Char('n') => self.begin_new_forward(),
            _ => {}
        }
    }
}
