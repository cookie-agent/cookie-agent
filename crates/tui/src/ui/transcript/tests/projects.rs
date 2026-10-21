//! Projects in the TUI: the bottom bar's project and its picker, a
//! session moved with `session.project.set`, a draft taking a project's
//! defaults, a project made from the picker, `/project`, and the session
//! picker's project filter (Ctrl+P).

use std::sync::{Arc, Mutex};

use cookie_agent_protocol::{
    AgentId, CapabilityName, ProjectDefaults, ProjectId, ProjectInfo, ProjectName, ProjectRuntime,
    RuntimeId, SessionId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::ui::app::*;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

pub(super) fn project(id: &str, name: &str, emoji: Option<&str>) -> ProjectInfo {
    ProjectInfo {
        id: ProjectId::new(id).unwrap(),
        name: ProjectName::new(name).unwrap(),
        emoji: emoji.map(str::to_owned),
        revision: "r1".into(),
        defaults: None,
        has_agents_md: false,
        sessions: 0,
        memories: 1,
    }
}

fn requests(recorded: &Arc<Mutex<Vec<Value>>>, method: &str) -> Vec<Value> {
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .filter(|value| value["method"].as_str() == Some(method))
        .map(|value| value["params"].clone())
        .collect()
}

/// An idle viewed session in project `web` (🍪), another in none, and a
/// daemon of client protocol 1.8 that moves sessions and creates projects.
pub(super) async fn project_app() -> (App, SessionId, SessionId, Arc<Mutex<Vec<Value>>>) {
    let mut app = test_app().await;
    let viewed = SessionId::new_v7();
    let other = SessionId::new_v7();
    let mut in_web = titled_meta(viewed, "viewed session", 1);
    in_web.project = Some(ProjectId::new("web1").unwrap());
    app.sessions = vec![in_web, titled_meta(other, "other session", 1)];
    app.selected = Some(viewed);
    app.tree_root = Some(viewed);
    // In name order, as `projects.list` has them.
    app.projects = vec![
        project("api1", "api", None),
        project("web1", "web", Some("🍪")),
    ];
    let (client, recorded) = projects_client().await;
    app.client = client;
    (app, viewed, other, recorded)
}

/// A daemon of client protocol 1.8 that moves sessions and creates
/// projects, handshaken.
pub(super) async fn projects_client() -> (cookie_agent_protocol::Client, Arc<Mutex<Vec<Value>>>) {
    let (client, recorded) = answering_client(move |method, params| match method {
        "handshake" => Ok(json!({
            "protocol_version": "1.8.0",
            "engine_version": "0.3.0",
            "origin": "client:tui-0192f3a45b6c7d8e9f00112233445566",
            "features": []
        })),
        "session.project.set" => {
            let session_id = params["session_id"].as_str().unwrap().parse().unwrap();
            let mut meta = titled_meta(session_id, "moved", 1);
            meta.project = params["project"]
                .as_str()
                .map(|id| ProjectId::new(id).unwrap());
            Ok(json!({ "session": serde_json::to_value(meta).unwrap() }))
        }
        "project.create" => {
            let mut created = project("new1", params["name"].as_str().unwrap(), None);
            created.revision = "r0".into();
            Ok(json!({ "project": serde_json::to_value(created).unwrap() }))
        }
        "projects.list" => Ok(json!({ "projects": [
            serde_json::to_value(project("api1", "api", None)).unwrap(),
            serde_json::to_value(project("web1", "web", Some("🍪"))).unwrap(),
        ] })),
        "session.list" => Ok(json!({ "sessions": [] })),
        _ => Err(json!({"code": -32601, "message": "not scripted"})),
    });
    client.handshake().await.expect("handshake");
    (client, recorded)
}

async fn next_update(app: &mut App) {
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
        .await
        .expect("rpc update timeout")
        .expect("rpc update");
    app.handle_rpc_update(update);
}

/// Handles updates until the status stops saying `pending`.
async fn settled(app: &mut App, pending: &str) {
    loop {
        next_update(app).await;
        if !app.status.starts_with(pending) {
            break;
        }
    }
}

fn bar(app: &mut App) -> String {
    frame_rows(app, 100, 30).last().cloned().unwrap_or_default()
}

/// Clicks the bottom bar's project.
async fn click_project(app: &mut App) {
    frame_rows(app, 100, 30);
    let label = app.hit_map.project_label.expect("the project label");
    app.handle_click(label.x, label.y).await;
}

#[tokio::test]
async fn the_bar_starts_with_the_project_while_no_run_is_active() {
    let (mut app, _, _, _) = project_app().await;
    let line = bar(&mut app);
    assert!(
        line.starts_with('🍪') && line.contains("web · "),
        "{line:?}"
    );
    // The emoji takes two cells, and the label's target covers them all.
    let label = app.hit_map.project_label.expect("the project label");
    assert_eq!(label.width, 6);
    assert_eq!(
        app.pointer_target_at(label.x + 5, label.y),
        Some(PointerTarget::ProjectLabel)
    );
    assert_eq!(
        app.hover_target_at(label.x, label.y),
        Some(PointerTarget::ProjectLabel)
    );

    // A draft in no project says so.
    app.new_session_draft = app.default_draft_selection();
    let line = bar(&mut app);
    assert!(line.starts_with("no project · "), "{line:?}");

    // An older daemon has no projects, so the bar names none.
    let (client, _) = answering_client(|_, _| Err(json!({"code": -32601, "message": "no"})));
    app.client = client;
    let line = bar(&mut app);
    assert!(!line.contains("project"), "{line:?}");
    assert!(app.hit_map.project_label.is_none());
}

#[tokio::test]
async fn a_running_session_shows_its_work_not_its_project() {
    let (mut app, session, _) = app_with_active_run().await;
    let mut meta = titled_meta(session, "running", 1);
    meta.project = Some(ProjectId::new("web1").unwrap());
    app.sessions = vec![meta];
    app.projects = vec![project("web1", "web", Some("🍪"))];
    app.client = projects_client().await.0;
    let line = bar(&mut app);
    assert!(!line.contains("web"), "{line:?}");
    assert!(line.contains("working"), "{line:?}");
    assert!(app.hit_map.project_label.is_none());
}

#[tokio::test]
async fn the_picker_moves_the_loaded_session_and_keeps_its_attachments() {
    let (mut app, viewed, _, recorded) = project_app().await;
    click_project(&mut app).await;
    assert_eq!(app.modal, Modal::Projects);
    let labels = app.project_picker_labels();
    assert_eq!(
        labels,
        ["api", "🍪 web  (current)", "No project", "New project…"]
    );
    // The current project is highlighted.
    assert_eq!(app.picker_state.selected(), Some(1));
    app.handle_key(key(KeyCode::Up)).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.modal, Modal::None);
    settled(&mut app, "Moving").await;
    let sent = requests(&recorded, "session.project.set");
    assert_eq!(
        sent,
        [json!({"session_id": viewed.to_string(), "project": "api1"})]
    );
    assert_eq!(app.status, "Moved the session to api.");
    let line = bar(&mut app);
    assert!(line.starts_with("api · "), "{line:?}");

    // No project moves it out.
    click_project(&mut app).await;
    app.choose_project_entry(2);
    settled(&mut app, "Moving").await;
    let sent = requests(&recorded, "session.project.set");
    assert_eq!(sent[1]["project"], Value::Null);
}

#[tokio::test]
async fn a_draft_takes_the_projects_attachments_and_agent() {
    let (mut app, _, _, recorded) = project_app().await;
    let mut builder = descriptor("builder", true);
    builder.preset = Some("rust".into());
    app.agents.push(builder);
    app.new_session_draft = app.default_draft_selection();
    app.projects[0].defaults = Some(ProjectDefaults {
        preset: Some("rust".into()),
        agent: Some(AgentId::new("builder").unwrap()),
        global: true,
        runtimes: vec![ProjectRuntime {
            runtime_id: RuntimeId::new("labpc").unwrap(),
            capability: Some(CapabilityName::coding()),
            workspace: Some("/srv/api".into()),
        }],
    });
    app.open_project_picker();
    app.choose_project_entry(0);
    assert_eq!(
        app.draft_project
            .as_ref()
            .map(|project| project.name.as_str()),
        Some("api")
    );
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("builder")
    );
    assert_eq!(app.selected_preset.as_deref(), Some("rust"));
    // Nothing is sent before the first message; the bar shows where the
    // defaults will attach.
    assert!(requests(&recorded, "session.project.set").is_empty());
    let line = bar(&mut app);
    assert!(line.starts_with("api · "), "{line:?}");
    assert!(line.contains("labpc · /srv/api"), "{line:?}");

    // A project naming an agent that cannot start a session keeps the
    // draft's agent, and says so.
    app.projects[1].defaults = Some(ProjectDefaults {
        agent: Some(AgentId::new("gone").unwrap()),
        ..ProjectDefaults::default()
    });
    app.new_session_draft = app.default_draft_selection();
    app.open_project_picker();
    app.choose_project_entry(1);
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("primary")
    );
    assert!(app.status.contains("names agent gone"), "{}", app.status);
}

#[tokio::test]
async fn new_project_asks_for_a_name_then_picks_it() {
    let (mut app, _, _, recorded) = project_app().await;
    app.new_session_draft = app.default_draft_selection();
    app.open_command_palette();
    for character in "project".chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.modal, Modal::Projects);
    app.choose_project_entry(3);
    assert!(app.project_create.is_some());
    rendered_frame(&mut app, 100, 30);
    // The field is a text field: no target in it.
    for character in "Bad Name".chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(
        app.status.starts_with("A project name is"),
        "{}",
        app.status
    );
    assert!(requests(&recorded, "project.create").is_empty());
    app.project_create = Some(Default::default());
    for character in "docs".chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
    app.handle_key(key(KeyCode::Enter)).await;
    settled(&mut app, "Creating").await;
    assert_eq!(
        requests(&recorded, "project.create"),
        [json!({"name": "docs"})]
    );
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.draft_project
            .as_ref()
            .map(|project| project.name.as_str()),
        Some("docs")
    );
    assert!(
        app.projects
            .iter()
            .any(|project| project.name.as_str() == "docs")
    );
}

#[tokio::test]
async fn the_session_picker_shows_and_filters_by_project() {
    let (mut app, viewed, other, _) = project_app().await;
    app.modal = Modal::Sessions;
    frame_rows(&mut app, 100, 30);
    let rows = app.current_session_search_rows().to_vec();
    let label = |session: SessionId| {
        rows.iter()
            .find_map(|row| match row {
                crate::ui::pickers::SessionSearchRow::Session { session_id, label }
                    if *session_id == session =>
                {
                    Some(label.clone())
                }
                _ => None,
            })
            .expect("listed")
    };
    assert!(
        label(viewed).contains("(🍪 web · primary"),
        "{}",
        label(viewed)
    );
    assert!(!label(other).contains("web"), "{}", label(other));

    // Ctrl+P: each project in name order, then no project, then all.
    app.handle_key(ctrl('p')).await;
    assert_eq!(app.session_search_ids(), Vec::<SessionId>::new());
    assert_eq!(app.session_project_filter_label(), "api");
    app.handle_key(ctrl('p')).await;
    assert_eq!(app.session_search_ids(), [viewed]);
    let frame = rendered_frame(&mut app, 160, 30);
    assert!(frame.contains("Sessions — this directory · 🍪"), "{frame}");
    assert!(frame.contains("web (ctrl+p)"), "{frame}");
    app.handle_key(ctrl('p')).await;
    assert_eq!(app.session_search_ids(), [other]);
    app.handle_key(ctrl('p')).await;
    assert_eq!(app.session_search_ids().len(), 2);
    assert_eq!(app.session_project_filter_label(), "all projects");
}
