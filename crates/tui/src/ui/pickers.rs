//! Session/agent/provider picker presentation, filtering, and tree flattening.

use std::collections::HashSet;

use cookie_agent_protocol::{ProviderDescriptor, SessionId, SessionMeta, SessionTree};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

use crate::theme::Theme;

use super::input::{self, InputState, RenderedInput};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SearchPickerFocus {
    #[default]
    Input,
    List,
}

/// Reusable single-line search state for picker panels.
#[derive(Default)]
pub(crate) struct SearchPickerState {
    input: InputState,
    focus: SearchPickerFocus,
}

impl SearchPickerState {
    pub(crate) fn query(&self) -> &str {
        self.input.as_str()
    }

    pub(crate) fn input_mut(&mut self) -> &mut InputState {
        &mut self.input
    }

    pub(crate) fn focus(&self) -> SearchPickerFocus {
        self.focus
    }

    pub(crate) fn focus_input(&mut self) {
        self.focus = SearchPickerFocus::Input;
    }

    pub(crate) fn focus_list(&mut self) {
        self.focus = SearchPickerFocus::List;
    }

    pub(crate) fn reset(&mut self) {
        self.input.set_buffer(String::new());
        self.focus_input();
    }
}

pub(crate) fn render_search_input(
    frame: &mut Frame,
    area: Rect,
    state: &mut SearchPickerState,
    theme: &Theme,
) -> RenderedInput {
    let title = match state.focus {
        SearchPickerFocus::Input => "Search · Down/Tab/Enter: results",
        SearchPickerFocus::List => "Search · Esc/BackTab: edit",
    };
    input::render(
        frame,
        area,
        &mut state.input,
        state.focus == SearchPickerFocus::Input,
        title,
        Some("Filter…"),
        theme,
    )
}

/// Case-insensitive substring matching over provider ID and display name.
pub(crate) fn provider_matches(provider: &ProviderDescriptor, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    provider
        .display_name
        .as_str()
        .to_lowercase()
        .contains(&query)
        || provider.id.as_str().to_lowercase().contains(&query)
}

/// Session picker matching over title and the untitled placeholder.
pub(crate) fn session_matches(session: &SessionMeta, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    session
        .title
        .as_ref()
        .map_or("untitled", |title| title.as_str())
        .to_lowercase()
        .contains(&query)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionSearchRow {
    Header(String),
    Session {
        session_id: SessionId,
        label: String,
    },
}

impl SessionSearchRow {
    pub(crate) fn session_id(&self) -> Option<SessionId> {
        match self {
            Self::Header(_) => None,
            Self::Session { session_id, .. } => Some(*session_id),
        }
    }
}

/// Search sessions by title and group matching rows by their local activity day.
pub(crate) fn session_search_rows(
    sessions: &[SessionMeta],
    query: &str,
    now: Timestamp,
    time_zone: &TimeZone,
) -> Vec<SessionSearchRow> {
    let today = now.to_zoned(time_zone.clone()).date();
    let yesterday = today.yesterday().ok();
    let mut sessions = sessions
        .iter()
        .filter(|session| session_matches(session, query))
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity));

    let mut rows = Vec::new();
    let mut current_date = None;
    for session in sessions {
        let date = session.last_activity.to_zoned(time_zone.clone()).date();
        if current_date != Some(date) {
            rows.push(SessionSearchRow::Header(session_day_label(
                date, today, yesterday,
            )));
            current_date = Some(date);
        }
        let title = session
            .title
            .as_ref()
            .map_or_else(|| "untitled".to_owned(), ToString::to_string);
        rows.push(SessionSearchRow::Session {
            session_id: session.session_id,
            label: format!(
                "{title}  ({} · {})",
                session.creation_selection.agent,
                short_id(session.session_id)
            ),
        });
    }
    rows
}

fn session_day_label(date: Date, today: Date, yesterday: Option<Date>) -> String {
    if date == today {
        "Today".to_owned()
    } else if Some(date) == yesterday {
        "Yesterday".to_owned()
    } else {
        date.strftime("%b %-d").to_string()
    }
}

/// First eight characters of a session ID for subdued secondary display.
pub(crate) fn short_id(session_id: SessionId) -> String {
    session_id.to_string().chars().take(8).collect()
}

/// A right-aligned, dimmed key hint on a picker panel's bottom border, so
/// every chooser explains itself without docs.
fn footer_hint(theme: &Theme, hint: Option<&str>) -> Option<Line<'static>> {
    hint.map(|hint| Line::from(Span::styled(hint.to_owned(), theme.internal())).right_aligned())
}

/// Rows never hard-clip mid-word at the panel edge: each label is ellipsized
/// to the space left after the selection marker's two columns.
fn ellipsized(entries: Vec<String>, inner_width: u16) -> Vec<String> {
    let available = usize::from(inner_width.saturating_sub(2));
    entries
        .into_iter()
        .map(|entry| super::app::truncate_with_ellipsis(&entry, available))
        .collect()
}

/// The textual chrome of a picker panel: its title, the message shown when
/// there is nothing to list, and the bottom-border key hint.
pub(crate) struct PickerChrome<'a> {
    pub(crate) title: &'a str,
    pub(crate) empty_message: Option<&'a str>,
    pub(crate) hint: Option<&'a str>,
}

pub(crate) fn render(
    frame: &mut Frame,
    chrome: PickerChrome<'_>,
    entries: Vec<String>,
    area: Rect,
    state: &mut ListState,
    theme: &Theme,
) -> Vec<(Rect, usize)> {
    super::app::paint_panel(frame, area, theme);
    let entry_count = entries.len();
    if entry_count == 0 {
        let content = chrome.empty_message.map_or_else(
            || {
                Line::from(vec![
                    Span::styled("No matches. ", theme.muted()),
                    Span::styled("Backspace or Ctrl-U clears the filter.", theme.internal()),
                ])
            },
            |message| Line::from(Span::styled(message.to_owned(), theme.muted())),
        );
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border())
            .title(chrome.title);
        if let Some(hint) = footer_hint(theme, chrome.hint) {
            block = block.title_bottom(hint);
        }
        frame.render_widget(ratatui::widgets::Paragraph::new(content).block(block), area);
        return Vec::new();
    }
    let inner = inner_rect(area);
    let entries = ellipsized(entries, inner.width);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border())
        .title(chrome.title);
    if let Some(hint) = footer_hint(theme, chrome.hint) {
        block = block.title_bottom(hint);
    }
    frame.render_stateful_widget(
        List::new(entries.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(block),
        area,
        state,
    );
    (state.offset()..entry_count)
        .take(usize::from(inner.height))
        .enumerate()
        .map(|(row, index)| {
            (
                Rect::new(
                    inner.x,
                    inner.y + u16::try_from(row).unwrap_or(u16::MAX),
                    inner.width,
                    1,
                ),
                index,
            )
        })
        .collect()
}

pub(crate) fn move_selection(state: &mut ListState, len: usize, up: bool) {
    if len == 0 {
        state.select(None);
        return;
    }
    let selected = state.selected().unwrap_or(0);
    state.select(Some(if up {
        selected.saturating_sub(1)
    } else {
        (selected + 1).min(len - 1)
    }));
}

pub(crate) fn cycle_selection(state: &mut ListState, len: usize, backward: bool) {
    if len == 0 {
        state.select(None);
        return;
    }
    let selected = state.selected().unwrap_or(0) % len;
    state.select(Some(if backward {
        (selected + len - 1) % len
    } else {
        (selected + 1) % len
    }));
}

pub(crate) fn clamp_tree_view(
    selection: &mut usize,
    offset: &mut usize,
    entry_count: usize,
    viewport_height: usize,
) {
    if entry_count == 0 {
        *selection = 0;
        *offset = 0;
        return;
    }
    *selection = (*selection).min(entry_count - 1);
    if viewport_height == 0 {
        *offset = 0;
        return;
    }
    let max_offset = entry_count.saturating_sub(viewport_height);
    *offset = (*offset).min(max_offset);
    if *selection < *offset {
        *offset = *selection;
    } else if *selection >= *offset + viewport_height {
        *offset = (*selection + 1).saturating_sub(viewport_height);
    }
}

/// Depth-first flattening of the delegation tree with each node's depth.
pub(crate) fn flatten_tree(
    tree: &SessionTree,
    depth: usize,
    collapsed: &HashSet<SessionId>,
    entries: &mut Vec<(SessionId, SessionMeta, usize)>,
) {
    entries.push((tree.session.session_id, tree.session.clone(), depth));
    if !collapsed.contains(&tree.session.session_id) {
        for child in &tree.children {
            flatten_tree(child, depth + 1, collapsed, entries);
        }
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AgentId, ModelSelection, ProviderDescriptor, RunSelection, SessionId, SessionMeta,
        SessionMetaSchemaVersion, SessionOrigin, SessionStatus,
    };

    use ratatui::widgets::ListState;

    use super::{
        SessionSearchRow, clamp_tree_view, cycle_selection, move_selection, provider_matches,
        session_matches, session_search_rows, short_id,
    };

    fn provider() -> ProviderDescriptor {
        serde_json::from_value(serde_json::json!({
            "id": "acme-ai",
            "display_name": "Acme AI",
            "presence": "current",
            "support": {"state": "supported", "reason": null},
            "setup_fields": [{
                "id": "region",
                "display_name": "Region",
                "help": "API region endpoint",
                "required": true,
                "default": "us-east-1",
                "validation": {"value_type": "string", "min_length": 1, "max_length": 32, "minimum": null, "maximum": null},
                "safe_to_project": true
            }],
            "auth_methods": [{
                "id": "api-key",
                "display_name": "API key",
                "credentials": [{
                    "id": "api_key",
                    "display_name": "Acme API Key",
                    "help": "ACME_API_KEY credential",
                    "required": true,
                    "credential_type": "api_key"
                }]
            }],
            "configuration": "unconfigured",
            "effective_auth_state": "unavailable",
            "durable_connection": null,
            "quarantine": null
        }))
        .expect("provider descriptor")
    }

    fn revision<T>(value: &str) -> T
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::json!(format!("sha256:{}", value.repeat(64))))
            .expect("revision")
    }

    fn session_meta(session_id: SessionId) -> SessionMeta {
        SessionMeta {
            meta_schema_version: SessionMetaSchemaVersion::current(),
            session_id,
            origin: SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace")
                .expect("cwd identity"),
            creation_selection: RunSelection {
                agent: AgentId::new("primary").expect("agent id"),
                model: ModelSelection {
                    model: "provider/model".parse().expect("model key"),
                    variant: None,
                },
            },
            runtime_revision: revision("1"),
            catalog_revision: revision("2"),
            provider_state_revision: revision("3"),
            model_revision: revision("4"),
            agent_revision: revision("5"),
            recipe_registry_revision: revision("6"),
            manifest_revision: revision("7"),
            title: None,
            title_updated_seq: 0,
            last_event_seq: 1,
            last_activity: "2026-08-06T12:00:00Z".parse().expect("timestamp"),
            status: SessionStatus::Idle,
        }
    }

    #[test]
    fn provider_filter_matches_id_and_display_name_case_insensitively() {
        let provider = provider();
        for query in ["acme", "ACME-AI", "me aI"] {
            assert!(provider_matches(&provider, query), "query {query}");
        }
        assert!(!provider_matches(&provider, "other-vendor"));
        assert!(!provider_matches(&provider, "region"));
        assert!(!provider_matches(&provider, "api_key"));
        assert!(provider_matches(&provider, ""));
    }

    #[test]
    fn empty_picker_navigation_keeps_selection_absent() {
        let mut state = ListState::default().with_selected(Some(0));

        move_selection(&mut state, 0, false);
        assert_eq!(state.selected(), None);

        state.select(Some(0));
        cycle_selection(&mut state, 0, false);
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn session_filter_matches_title_and_untitled_placeholder_only() {
        let session_id = SessionId::new_v7();
        let mut session = SessionMeta {
            session_id,
            title: Some(
                cookie_agent_protocol::SessionTitle::new("Quarterly report cleanup")
                    .expect("title"),
            ),
            ..session_meta(session_id)
        };
        assert!(session_matches(&session, "quarterly"));
        assert!(session_matches(&session, "REPORT"));
        assert!(!session_matches(&session, "primary"));
        assert!(!session_matches(&session, &session_id.to_string()));
        assert!(!session_matches(&session, "unrelated words"));
        session.title = None;
        assert!(!session_matches(&session, "quarterly"));
        assert!(session_matches(&session, "UNTITLED"));
    }

    #[test]
    fn session_search_groups_local_days_and_sorts_newest_first() {
        let now = "2026-08-06T12:00:00Z".parse().expect("timestamp");
        let time_zone = jiff::tz::TimeZone::UTC;
        let mut today_old = session_meta(SessionId::new_v7());
        today_old.title =
            Some(cookie_agent_protocol::SessionTitle::new("today old").expect("title"));
        today_old.last_activity = "2026-08-06T09:00:00Z".parse().expect("timestamp");
        let mut today_new = session_meta(SessionId::new_v7());
        today_new.title =
            Some(cookie_agent_protocol::SessionTitle::new("today new").expect("title"));
        today_new.last_activity = "2026-08-06T11:00:00Z".parse().expect("timestamp");
        let mut yesterday = session_meta(SessionId::new_v7());
        yesterday.title =
            Some(cookie_agent_protocol::SessionTitle::new("yesterday").expect("title"));
        yesterday.last_activity = "2026-08-05T18:00:00Z".parse().expect("timestamp");
        let mut older = session_meta(SessionId::new_v7());
        older.title = Some(cookie_agent_protocol::SessionTitle::new("older").expect("title"));
        older.last_activity = "2026-08-04T18:00:00Z".parse().expect("timestamp");

        let rows = session_search_rows(
            &[today_old, older, today_new, yesterday],
            "",
            now,
            &time_zone,
        );
        assert_eq!(rows[0], SessionSearchRow::Header("Today".into()));
        assert!(
            matches!(&rows[1], SessionSearchRow::Session { label, .. } if label.starts_with("today new"))
        );
        assert!(
            matches!(&rows[2], SessionSearchRow::Session { label, .. } if label.starts_with("today old"))
        );
        assert_eq!(rows[3], SessionSearchRow::Header("Yesterday".into()));
        assert!(
            matches!(&rows[4], SessionSearchRow::Session { label, .. } if label.starts_with("yesterday"))
        );
        assert_eq!(rows[5], SessionSearchRow::Header("Aug 4".into()));
        assert!(
            matches!(&rows[6], SessionSearchRow::Session { label, .. } if label.starts_with("older"))
        );
        assert!(rows.iter().filter_map(SessionSearchRow::session_id).count() == 4);
        assert!(
            rows.iter()
                .filter(|row| matches!(row, SessionSearchRow::Header(_)))
                .all(|row| row.session_id().is_none())
        );
    }

    #[test]
    fn short_id_is_subdued_metadata_not_the_full_identity() {
        let session_id = SessionId::new_v7();
        assert_eq!(short_id(session_id).len(), 8);
        assert!(session_id.to_string().starts_with(&short_id(session_id)));
    }

    #[test]
    fn picker_rows_ellipsize_and_the_footer_explains_the_keys() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = crate::theme::Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).expect("terminal");
        let mut state = ListState::default().with_selected(Some(0));
        terminal
            .draw(|frame| {
                super::render(
                    frame,
                    super::PickerChrome {
                        title: "Model",
                        empty_message: None,
                        hint: Some("↑↓ move · enter: select · esc: close"),
                    },
                    vec!["a/very/long/model-name[variant] — Display Name".to_owned()],
                    frame.area(),
                    &mut state,
                    &theme,
                );
            })
            .expect("render picker");
        let buffer = terminal.backend().buffer();
        let text = (0..8)
            .flat_map(|y| (0..30).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect::<String>();
        // The long row ellipsizes instead of clipping mid-word…
        assert!(text.contains('…'), "{text}");
        assert!(!text.contains("Display Name"), "{text}");
        // …and never spills over the right border.
        assert_eq!(buffer[(29, 1)].symbol(), "│");
        // The bottom border carries the key hints.
        assert!(text.contains("enter: select"), "{text}");
        assert!(text.contains("esc: close"), "{text}");
    }

    #[test]
    fn tree_view_keeps_selection_inside_the_visible_window() {
        let mut selection = 0;
        let mut offset = 0;
        clamp_tree_view(&mut selection, &mut offset, 20, 4);
        selection = 7;
        clamp_tree_view(&mut selection, &mut offset, 20, 4);
        assert_eq!(offset, 4);
        selection = 2;
        clamp_tree_view(&mut selection, &mut offset, 20, 4);
        assert_eq!(offset, 2);
        selection = 99;
        clamp_tree_view(&mut selection, &mut offset, 20, 4);
        assert_eq!((selection, offset), (19, 16));
    }

    #[test]
    fn empty_or_zero_height_tree_view_resets_state() {
        let mut selection = 8;
        let mut offset = 5;
        clamp_tree_view(&mut selection, &mut offset, 0, 4);
        assert_eq!((selection, offset), (0, 0));
        selection = 8;
        offset = 5;
        clamp_tree_view(&mut selection, &mut offset, 20, 0);
        assert_eq!((selection, offset), (8, 0));
    }
}
