use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{
    AgentId, AssistantToolCallRef, AttemptId, EventPayload, ModelCallId, ModelSelection,
    PersistedToolResult, ProviderDescriptor, RunSelection, SafeDisplayText, SessionId, SessionMeta,
    SessionOrigin, SessionStatus, StoredEvent, ToolCallId, ToolCallTermination,
    ToolTerminationOutcome,
};

use ratatui::widgets::ListState;

use super::{
    SessionSearchRow, clamp_tree_view, cycle_selection, flatten_tree, move_selection,
    provider_matches, session_matches, session_search_rows, short_id,
};
use crate::state::{SessionState, StateStore};

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
        session_id,
        origin: SessionOrigin::Root,
        short_id: None,
        cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace").expect("cwd identity"),
        creation_selection: RunSelection {
            agent: AgentId::new("primary").expect("agent id"),
            model: ModelSelection {
                model: "provider/model".parse().expect("model key"),
                variant: None,
            },
            preset: None,
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
        skipped_events: Vec::new(),
    }
}

fn session_id(ordinal: u8) -> SessionId {
    format!("018f0000-0000-7000-8000-{ordinal:012x}")
        .parse()
        .expect("session ID")
}

fn tree(
    session_id: SessionId,
    children: Vec<cookie_agent_protocol::SessionTree>,
) -> cookie_agent_protocol::SessionTree {
    cookie_agent_protocol::SessionTree {
        session: session_meta(session_id),
        children,
    }
}

fn state(created_at: &str, activity: Option<&str>) -> SessionState {
    SessionState {
        created_at: Some(created_at.parse().expect("creation timestamp")),
        last_agent_activity: activity.map(|value| value.parse().expect("activity timestamp")),
        ..SessionState::default()
    }
}

fn flattened(
    tree: &cookie_agent_protocol::SessionTree,
    states: &HashMap<SessionId, SessionState>,
) -> Vec<(SessionId, usize)> {
    let mut entries = Vec::new();
    flatten_tree(tree, 0, &HashSet::new(), states, &mut entries);
    entries
        .into_iter()
        .map(|(session_id, _, depth)| (session_id, depth))
        .collect()
}

fn stored_event(
    session_id: SessionId,
    seq: u64,
    timestamp: &str,
    payload: EventPayload,
) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: None,
        seq,
        timestamp: timestamp.parse().expect("event timestamp"),
        payload,
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
            cookie_agent_protocol::SessionTitle::new("Quarterly report cleanup").expect("title"),
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
    today_old.title = Some(cookie_agent_protocol::SessionTitle::new("today old").expect("title"));
    today_old.last_activity = "2026-08-06T09:00:00Z".parse().expect("timestamp");
    let mut today_new = session_meta(SessionId::new_v7());
    today_new.title = Some(cookie_agent_protocol::SessionTitle::new("today new").expect("title"));
    today_new.last_activity = "2026-08-06T11:00:00Z".parse().expect("timestamp");
    let mut yesterday = session_meta(SessionId::new_v7());
    yesterday.title = Some(cookie_agent_protocol::SessionTitle::new("yesterday").expect("title"));
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
fn short_id_prefers_the_real_handle_and_falls_back_to_the_uuid_prefix() {
    let session_id = SessionId::new_v7();
    let mut meta = session_meta(session_id);
    meta.short_id = Some("explore_1a2b3c4d".into());
    assert_eq!(short_id(&meta), "explore_1a2b3c4d");
    meta.short_id = None;
    assert_eq!(short_id(&meta).len(), 8);
    assert!(session_id.to_string().starts_with(&short_id(&meta)));
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
fn sibling_agents_sort_by_recent_activity_with_root_pinned() {
    let root = session_id(1);
    let older = session_id(2);
    let newer = session_id(3);
    let tree = tree(root, vec![tree(older, Vec::new()), tree(newer, Vec::new())]);
    let states = HashMap::from([
        (root, state("2026-08-06T10:00:00Z", None)),
        (
            older,
            state("2026-08-06T10:01:00Z", Some("2026-08-06T11:00:00Z")),
        ),
        (
            newer,
            state("2026-08-06T10:02:00Z", Some("2026-08-06T12:00:00Z")),
        ),
    ]);

    assert_eq!(
        flattened(&tree, &states),
        [(root, 0), (newer, 1), (older, 1)]
    );
}

#[test]
fn grandchildren_sort_only_within_their_parent() {
    let root = session_id(1);
    let first_parent = session_id(2);
    let second_parent = session_id(3);
    let first_old = session_id(4);
    let first_new = session_id(5);
    let second_child = session_id(6);
    let tree = tree(
        root,
        vec![
            tree(
                first_parent,
                vec![tree(first_old, Vec::new()), tree(first_new, Vec::new())],
            ),
            tree(second_parent, vec![tree(second_child, Vec::new())]),
        ],
    );
    let states = HashMap::from([
        (first_parent, state("2026-08-06T10:01:00Z", None)),
        (
            second_parent,
            state("2026-08-06T10:02:00Z", Some("2026-08-06T11:00:00Z")),
        ),
        (
            first_old,
            state("2026-08-06T10:03:00Z", Some("2026-08-06T11:30:00Z")),
        ),
        (
            first_new,
            state("2026-08-06T10:04:00Z", Some("2026-08-06T12:00:00Z")),
        ),
        (
            second_child,
            state("2026-08-06T10:05:00Z", Some("2026-08-06T13:00:00Z")),
        ),
    ]);

    assert_eq!(
        flattened(&tree, &states),
        [
            (root, 0),
            (second_parent, 1),
            (second_child, 2),
            (first_parent, 1),
            (first_new, 2),
            (first_old, 2),
        ]
    );
}

#[test]
fn activity_ties_fall_back_to_creation_order() {
    let root = session_id(1);
    let created_first = session_id(2);
    let created_second = session_id(3);
    let tree = tree(
        root,
        vec![
            tree(created_second, Vec::new()),
            tree(created_first, Vec::new()),
        ],
    );
    let tied = Some("2026-08-06T12:00:00Z");
    let states = HashMap::from([
        (created_first, state("2026-08-06T10:01:00Z", tied)),
        (created_second, state("2026-08-06T10:02:00Z", tied)),
    ]);

    assert_eq!(
        flattened(&tree, &states),
        [(root, 0), (created_first, 1), (created_second, 1)]
    );
}

#[test]
fn admitted_user_message_live_resorts_siblings() {
    let root = session_id(1);
    let first = session_id(2);
    let second = session_id(3);
    let tree = tree(
        root,
        vec![tree(first, Vec::new()), tree(second, Vec::new())],
    );
    let mut store = StateStore::default();
    store
        .sessions
        .insert(first, state("2026-08-06T10:01:00Z", None));
    store
        .sessions
        .insert(second, state("2026-08-06T10:02:00Z", None));
    assert_eq!(flattened(&tree, &store.sessions)[1].0, first);

    assert!(store.apply_event(stored_event(
        second,
        1,
        "2026-08-06T12:00:00Z",
        EventPayload::UserInputAdmitted {
            input: "new work".into(),
        },
    )));

    assert_eq!(flattened(&tree, &store.sessions)[1].0, second);
}

#[test]
fn assistant_and_tool_result_events_do_not_resort_siblings() {
    let root = session_id(1);
    let active = session_id(2);
    let background = session_id(3);
    let tree = tree(
        root,
        vec![tree(active, Vec::new()), tree(background, Vec::new())],
    );
    let mut store = StateStore::default();
    store.sessions.insert(
        active,
        state("2026-08-06T10:01:00Z", Some("2026-08-06T11:00:00Z")),
    );
    store.sessions.insert(
        background,
        state("2026-08-06T10:02:00Z", Some("2026-08-06T10:30:00Z")),
    );
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: ModelCallId::new("background-call").expect("model call ID"),
        provider_item_id: None,
    };
    assert!(store.apply_event(stored_event(
        background,
        1,
        "2026-08-06T12:00:00Z",
        EventPayload::TextDelta {
            attempt_id: AttemptId::new_v7(),
            text: "background output".into(),
        },
    )));
    assert!(store.apply_event(stored_event(
        background,
        2,
        "2026-08-06T12:01:00Z",
        EventPayload::ToolCallTerminated {
            termination: ToolCallTermination {
                tool_call_id: ToolCallId::new_v7(),
                owner,
                outcome: ToolTerminationOutcome::Completed,
                result: Some(PersistedToolResult {
                    display: None,
                    retained_output: None,
                    title: SafeDisplayText::new("completed").expect("title"),
                    output: "result".into(),
                    metadata: serde_json::Value::Null,
                    truncation: None,
                    attachments: Vec::new(),
                    additional_messages: Vec::new(),
                }),
                error: None,
            },
        },
    )));

    assert_eq!(flattened(&tree, &store.sessions)[1].0, active);
    assert_eq!(
        store.sessions[&background].last_agent_activity,
        Some("2026-08-06T10:30:00Z".parse().expect("timestamp"))
    );
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
