//! Session/profile/provider picker presentation, filtering, and tree flattening.

use std::collections::HashSet;

use cookie_agent_protocol::{CatalogProvider, SessionId, SessionMeta, SessionTree};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::theme::Theme;

/// A provider row that matched the picker query, plus a subdued annotation
/// naming the non-name field that produced the match.
pub(crate) struct ProviderMatch<'a> {
    pub(crate) provider: &'a CatalogProvider,
    pub(crate) label: String,
}

/// Case-insensitive substring matching over provider name, ID, documentation
/// URL, endpoint, and credential field labels.
pub(crate) fn provider_matches<'a>(
    provider: &'a CatalogProvider,
    query: &str,
) -> Option<ProviderMatch<'a>> {
    if query.trim().is_empty() {
        return Some(ProviderMatch {
            provider,
            label: String::new(),
        });
    }
    let query = query.trim().to_lowercase();
    if provider.name.to_lowercase().contains(&query) || provider.id.to_lowercase().contains(&query)
    {
        return Some(ProviderMatch {
            provider,
            label: String::new(),
        });
    }
    if provider
        .documentation_url
        .as_deref()
        .is_some_and(|url| url.to_lowercase().contains(&query))
    {
        return Some(ProviderMatch {
            provider,
            label: " · docs".into(),
        });
    }
    if provider
        .api
        .as_deref()
        .is_some_and(|api| api.to_lowercase().contains(&query))
    {
        return Some(ProviderMatch {
            provider,
            label: " · endpoint".into(),
        });
    }
    if let Some(field) = provider
        .credential_fields
        .iter()
        .find(|field| field.to_lowercase().contains(&query))
    {
        return Some(ProviderMatch {
            provider,
            label: format!(" · field: {field}"),
        });
    }
    None
}

/// Session picker matching over title, profile name, and full session ID.
pub(crate) fn session_matches(session: &SessionMeta, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    session
        .title
        .as_ref()
        .is_some_and(|title| title.to_string().to_lowercase().contains(&query))
        || session.profile.name.to_lowercase().contains(&query)
        || session.id.to_string().to_lowercase().contains(&query)
}

/// First eight characters of a session ID for subdued secondary display.
pub(crate) fn short_id(session_id: SessionId) -> String {
    session_id.to_string().chars().take(8).collect()
}

pub(crate) fn render(
    frame: &mut Frame,
    title: &str,
    entries: Vec<String>,
    area: Rect,
    state: &mut ListState,
    theme: &Theme,
) -> Vec<(Rect, usize)> {
    frame.render_widget(Clear, area);
    let entry_count = entries.len();
    if entry_count == 0 {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(vec![
                Span::styled("No matches. ", theme.muted()),
                Span::styled("Backspace or Ctrl-U clears the filter.", theme.internal()),
            ]))
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
        state.select(Some(0));
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
        state.select(Some(0));
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
    entries.push((tree.session.id, tree.session.clone(), depth));
    if !collapsed.contains(&tree.session.id) {
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
    use cookie_agent_protocol::{CatalogProvider, SessionId, SessionMeta, SessionTitle};

    use super::{clamp_tree_view, provider_matches, session_matches, short_id};

    fn provider() -> CatalogProvider {
        CatalogProvider {
            id: "acme-ai".into(),
            name: "Acme AI".into(),
            credential_fields: vec!["ACME_API_KEY".into()],
            npm: None,
            api: Some("https://api.acme.example".into()),
            documentation_url: Some("https://docs.acme.example/setup".into()),
        }
    }

    fn session_meta(session_id: SessionId) -> SessionMeta {
        cookie_agent_protocol::SessionMeta {
            id: session_id,
            origin: cookie_agent_protocol::SessionOrigin::Root,
            cwd: "/workspace".into(),
            profile: cookie_agent_protocol::ProfileSnapshot {
                name: "primary".into(),
                agent_type: cookie_agent_protocol::AgentType::Primary,
                models: Vec::new(),
                tools: Vec::new(),
                delegation: cookie_agent_protocol::DelegationSnapshot {
                    enabled: false,
                    allowed_profiles: Vec::new(),
                    depth_limit: cookie_agent_protocol::DepthLimit::Finite(0),
                    result_limit_bytes: 0,
                },
                permission_rules: Vec::new(),
            },
            title: None,
        }
    }

    #[test]
    fn provider_filter_matches_name_id_docs_endpoint_and_credential_labels() {
        let provider = provider();
        for query in [
            "acme",
            "ACME-AI",
            "docs.acme",
            "api.acme",
            "api_key",
            "ACME_API",
        ] {
            assert!(
                provider_matches(&provider, query).is_some(),
                "query {query}"
            );
        }
        assert!(provider_matches(&provider, "other-vendor").is_none());
        assert_eq!(
            provider_matches(&provider, "api_key")
                .expect("field match")
                .label,
            " · field: ACME_API_KEY"
        );
        assert_eq!(provider_matches(&provider, "").expect("empty").label, "");
    }

    #[test]
    fn session_filter_matches_title_profile_and_full_id_but_not_short_id() {
        let session_id = SessionId::new_v7();
        let mut session = SessionMeta {
            id: session_id,
            title: Some(SessionTitle::new("Quarterly report cleanup").expect("title")),
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
