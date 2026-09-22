//! Session/agent/provider picker presentation, filtering, and tree flattening.

use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{
    AgentDescriptor, AvailableModelDescriptor, ModelSelection, ProviderDescriptor, SessionId,
    SessionMeta, SessionTree,
};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{List, ListItem, ListState},
};

use crate::{state::SessionState, theme::Theme};

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

/// Case-insensitive substring matching over agent IDs and descriptions.
pub(crate) fn agent_matches(agent: &AgentDescriptor, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    agent.id.as_str().to_lowercase().contains(&query)
        || agent.description.to_lowercase().contains(&query)
}

/// Case-insensitive substring matching over model display names and canonical keys.
pub(crate) fn model_matches(
    selection: &ModelSelection,
    descriptor: Option<&AvailableModelDescriptor>,
    query: &str,
) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    selection.model.to_string().to_lowercase().contains(&query)
        || descriptor.is_some_and(|descriptor| {
            descriptor.display_name.to_lowercase().contains(&query)
                || selection.variant.as_ref().is_some_and(|selected| {
                    descriptor.variants.iter().any(|variant| {
                        variant.id == *selected
                            && variant.display_name.to_lowercase().contains(&query)
                    })
                })
        })
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
/// Callers pass the sessions the picker may offer (root sessions only).
pub(crate) fn session_search_rows<'a>(
    sessions: impl IntoIterator<Item = &'a SessionMeta>,
    query: &str,
    now: Timestamp,
    time_zone: &TimeZone,
) -> Vec<SessionSearchRow> {
    let today = now.to_zoned(time_zone.clone()).date();
    let yesterday = today.yesterday().ok();
    let mut sessions = sessions
        .into_iter()
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
        let degraded = if session.skipped_events.is_empty() {
            ""
        } else {
            " !"
        };
        rows.push(SessionSearchRow::Session {
            session_id: session.session_id,
            label: format!(
                "{title}{degraded}  ({} · {})",
                session.creation_selection.agent,
                short_id(session)
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

/// The session's short handle for subdued secondary display, falling back to
/// the first eight characters of the UUID for pre-handle sessions.
pub(crate) fn short_id(meta: &SessionMeta) -> String {
    meta.short_id
        .clone()
        .unwrap_or_else(|| meta.session_id.to_string().chars().take(8).collect())
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
    let entries = ellipsized(entries, inner_rect(area).width);
    render_lines(
        frame,
        chrome,
        entries.into_iter().map(Line::from).collect(),
        area,
        state,
        theme,
        theme.selected(),
    )
}

pub(crate) fn render_lines(
    frame: &mut Frame,
    chrome: PickerChrome<'_>,
    entries: Vec<Line<'static>>,
    area: Rect,
    state: &mut ListState,
    theme: &Theme,
    highlight_style: Style,
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
        let mut block = crate::ui::panel_block()
            .border_style(theme.panel_border())
            .title(crate::ui::fitted_panel_title(chrome.title, area.width));
        if let Some(hint) = footer_hint(theme, chrome.hint) {
            block = block.title_bottom(crate::ui::fitted_panel_title(hint, area.width));
        }
        frame.render_widget(ratatui::widgets::Paragraph::new(content).block(block), area);
        return Vec::new();
    }
    let inner = inner_rect(area);
    let mut block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::fitted_panel_title(chrome.title, area.width));
    if let Some(hint) = footer_hint(theme, chrome.hint) {
        block = block.title_bottom(crate::ui::fitted_panel_title(hint, area.width));
    }
    frame.render_stateful_widget(
        List::new(entries.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("> ")
            .highlight_style(highlight_style)
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
    states: &HashMap<SessionId, SessionState>,
    entries: &mut Vec<(SessionId, SessionMeta, usize)>,
) {
    entries.push((tree.session.session_id, tree.session.clone(), depth));
    if !collapsed.contains(&tree.session.session_id) {
        let mut children = tree.children.iter().collect::<Vec<_>>();
        children.sort_by(|left, right| {
            let left_state = states.get(&left.session.session_id);
            let right_state = states.get(&right.session.session_id);
            right_state
                .and_then(|state| state.last_agent_activity)
                .cmp(&left_state.and_then(|state| state.last_agent_activity))
                .then_with(|| {
                    match (
                        left_state.and_then(|state| state.created_at),
                        right_state.and_then(|state| state.created_at),
                    ) {
                        (Some(left), Some(right)) => left.cmp(&right),
                        _ => left.session.session_id.cmp(&right.session.session_id),
                    }
                })
                .then_with(|| left.session.session_id.cmp(&right.session.session_id))
        });
        for child in children {
            flatten_tree(child, depth + 1, collapsed, states, entries);
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
mod tests;
