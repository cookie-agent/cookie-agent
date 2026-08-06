//! Session/agent/provider picker presentation, filtering, and tree flattening.

use std::collections::HashSet;

use cookie_agent_protocol::{ProviderDescriptor, SessionId, SessionMeta, SessionTree};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
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

/// Session picker matching over title, agent ID, and full session ID.
pub(crate) fn session_matches(session: &SessionMeta, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    session
        .title
        .as_ref()
        .is_some_and(|title| title.to_string().to_lowercase().contains(&query))
        || session
            .creation_selection
            .agent
            .as_str()
            .to_lowercase()
            .contains(&query)
        || session
            .session_id
            .to_string()
            .to_lowercase()
            .contains(&query)
}

/// First eight characters of a session ID for subdued secondary display.
pub(crate) fn short_id(session_id: SessionId) -> String {
    session_id.to_string().chars().take(8).collect()
}

pub(crate) fn render(
    frame: &mut Frame,
    title: &str,
    entries: Vec<String>,
    empty_message: Option<&str>,
    area: Rect,
    state: &mut ListState,
    theme: &Theme,
) -> Vec<(Rect, usize)> {
    frame.render_widget(Clear, area);
    let entry_count = entries.len();
    if entry_count == 0 {
        let content = empty_message.map_or_else(
            || {
                Line::from(vec![
                    Span::styled("No matches. ", theme.muted()),
                    Span::styled("Backspace or Ctrl-U clears the filter.", theme.internal()),
                ])
            },
            |message| Line::from(Span::styled(message.to_owned(), theme.muted())),
        );
        frame.render_widget(
            ratatui::widgets::Paragraph::new(content)
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return Vec::new();
    }
    frame.render_stateful_widget(
        List::new(entries.into_iter().map(ListItem::new).collect::<Vec<_>>())
            .highlight_symbol("> ")
            .highlight_style(theme.user())
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
        state,
    );
    let inner = inner_rect(area);
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
        clamp_tree_view, cycle_selection, move_selection, provider_matches, session_matches,
        short_id,
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
    fn session_filter_matches_title_agent_and_full_id_but_not_short_id() {
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
        assert!(session_matches(&session, "primary"));
        assert!(session_matches(&session, &session_id.to_string()));
        assert!(!session_matches(&session, "unrelated words"));
        session.title = None;
        assert!(!session_matches(&session, "quarterly"));
        assert!(session_matches(&session, "primary"));
    }

    #[test]
    fn short_id_is_subdued_metadata_not_the_full_identity() {
        let session_id = SessionId::new_v7();
        assert_eq!(short_id(session_id).len(), 8);
        assert!(session_id.to_string().starts_with(&short_id(session_id)));
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
