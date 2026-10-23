//! Projects as sessions and runs meet them (`docs/site/specs/projects.md`
//! sections 4 to 7): creation in a project and its defaults, moving
//! sessions, deleting a project, and what a root run's prompt shows of it.

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, MemorySnapshot, MemoryWriteParams, ProjectCreateParams,
    ProjectDefaults, ProjectDeleteParams, ProjectId, ProjectInfo, ProjectName, ProjectRuntime,
    ProjectUpdateParams, RunSelection, RunStartParams, RuntimeId, SessionId, SessionStatus,
};

use super::support::*;
use crate::{EngineError, ProjectError, SessionRuntimes};

fn origin() -> EventOrigin {
    EventOrigin::new("client:project-test").unwrap()
}

fn agent() -> String {
    "---\ndescription: Project test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: allow, memory_write: allow }\n---\nWork on projects.\n".to_owned()
}

fn name(value: &str) -> ProjectName {
    ProjectName::new(value).unwrap()
}

async fn create(
    fixture: &Fixture,
    project: &str,
    defaults: Option<ProjectDefaults>,
) -> ProjectInfo {
    fixture
        .engine
        .create_project(ProjectCreateParams {
            name: name(project),
            emoji: None,
            defaults,
        })
        .await
        .unwrap()
}

async fn project(fixture: &Fixture, id: &ProjectId) -> ProjectInfo {
    fixture
        .engine
        .get_project(id.clone())
        .await
        .unwrap()
        .project
}

async fn session_in(
    fixture: &Fixture,
    selection: &RunSelection,
    runtimes: SessionRuntimes,
    project: Option<ProjectId>,
) -> Result<SessionId, EngineError> {
    fixture
        .engine
        .create_session_in_project(selection.clone(), None, Vec::new(), runtimes, project)
        .await
        .map(|meta| meta.session_id)
}

/// Writes a memory as the user would from a client.
async fn user_write(fixture: &Fixture, url: &str, description: &str, body: &str) {
    let revision = fixture
        .engine
        .read_memory(cookie_agent_protocol::MemoryReadParams {
            url: url.to_owned(),
        })
        .await
        .ok()
        .map(|read| read.revision);
    fixture
        .engine
        .write_memory(MemoryWriteParams {
            url: url.to_owned(),
            text: format!("---\ndescription: {description}\n---\n{body}"),
            revision,
        })
        .await
        .unwrap();
}

async fn start(
    fixture: &Fixture,
    session: SessionId,
    selection: &RunSelection,
    id: &str,
    input: &str,
) {
    start_run_when_idle(
        &fixture.engine,
        RunStartParams {
            reset_fallback: false,
            session_id: session,
            client_run_id: ClientRunId::new(id).unwrap(),
            selection: selection.clone(),
            content: input.into(),
        },
        origin(),
    )
    .await
    .unwrap();
}

async fn run(
    fixture: &Fixture,
    session: SessionId,
    selection: &RunSelection,
    id: &str,
    input: &str,
) {
    start(fixture, session, selection, id, input).await;
    wait_for_session_not_running(&fixture.engine, session).await;
}

async fn wait_running(fixture: &Fixture, session: SessionId) {
    await_projection(
        &fixture.engine,
        session,
        "a running session",
        |projection| projection.status == SessionStatus::Running,
    )
    .await;
}

fn events(fixture: &Fixture, session: SessionId) -> Vec<cookie_agent_protocol::StoredEvent> {
    fixture
        .engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
}

fn snapshots(fixture: &Fixture, session: SessionId) -> Vec<MemorySnapshot> {
    events(fixture, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::MemoryLoaded { snapshot } => Some(snapshot),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_session_in_a_project_takes_its_default_attachments_unless_runtimes_are_given() {
    let (fixture, selection) = custom_fixture();
    let workspace = fixture._directory.path().join("web");
    create_private_test_dir(&workspace);
    let defaults = ProjectDefaults {
        runtimes: vec![ProjectRuntime {
            runtime_id: RuntimeId::local(),
            capability: Some(cookie_agent_protocol::CapabilityName::coding()),
            workspace: Some(workspace.to_string_lossy().into_owned()),
        }],
        ..ProjectDefaults::default()
    };
    let web = create(&fixture, "web", Some(defaults)).await;

    // No `runtimes`: the project's defaults.
    let session = session_in(
        &fixture,
        &selection,
        SessionRuntimes::default(),
        Some(web.id.clone()),
    )
    .await
    .unwrap();
    let attachments = fixture.engine.session_runtimes(session).unwrap();
    assert_eq!(attachments.len(), 2, "{attachments:?}");
    assert_eq!(attachments[0].runtime_id.as_str(), "global");
    let attached = std::path::PathBuf::from(attachments[1].workspace.as_deref().unwrap());
    assert_eq!(
        attached.canonicalize().unwrap(),
        workspace.canonicalize().unwrap()
    );
    let meta = fixture.engine.get_session(session).unwrap();
    assert_eq!(meta.project.as_ref(), Some(&web.id));
    // The creation recorded the membership.
    assert!(events(&fixture, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::SessionProjectSet { project } if project.as_ref() == Some(&web.id)
    )));
    assert_eq!(project(&fixture, &web.id).await.sessions, 1);

    // An explicit `runtimes`, even an empty one, wins.
    let explicit = session_in(
        &fixture,
        &selection,
        SessionRuntimes {
            runtimes: Some(Vec::new()),
            ..SessionRuntimes::default()
        },
        Some(web.id.clone()),
    )
    .await
    .unwrap();
    let attachments = fixture.engine.session_runtimes(explicit).unwrap();
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].runtime_id.as_str(), "global");
    assert_eq!(project(&fixture, &web.id).await.sessions, 2);

    // Defaults that only name an agent leave the usual attachments.
    let agent_only = create(
        &fixture,
        "agent-only",
        Some(ProjectDefaults {
            agent: Some(selection.agent.clone()),
            ..ProjectDefaults::default()
        }),
    )
    .await;
    let usual = session_in(
        &fixture,
        &selection,
        SessionRuntimes::default(),
        Some(agent_only.id),
    )
    .await
    .unwrap();
    let attachments = fixture.engine.session_runtimes(usual).unwrap();
    assert_eq!(
        attachments
            .iter()
            .map(|attachment| attachment.runtime_id.as_str())
            .collect::<Vec<_>>(),
        ["global", "local"]
    );

    // An unknown project is refused before anything is attached.
    let before = fixture.engine.list_sessions().len();
    let unknown = ProjectId::new("0000-unknown").unwrap();
    assert!(matches!(
        session_in(
            &fixture,
            &selection,
            SessionRuntimes::default(),
            Some(unknown)
        )
        .await,
        Err(EngineError::Project(ProjectError::NotFound(_)))
    ));
    assert_eq!(fixture.engine.list_sessions().len(), before);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_default_agent_must_be_listed_and_able_to_run_as_root() {
    let (fixture, selection) = custom_fixture();
    let web = create(&fixture, "web", None).await;
    let with_agent = |agent: &str, preset: Option<&str>| {
        let mut params = ProjectUpdateParams::new(web.id.clone(), web.revision.clone());
        params.defaults = Some(Some(ProjectDefaults {
            agent: Some(cookie_agent_protocol::AgentId::new(agent).unwrap()),
            preset: preset.map(str::to_owned),
            ..ProjectDefaults::default()
        }));
        params
    };
    for (agent, preset) in [("ghost", None), (selection.agent.as_str(), Some("missing"))] {
        let refused = fixture
            .engine
            .update_project(with_agent(agent, preset))
            .await;
        assert!(
            matches!(
                refused,
                Err(EngineError::Project(ProjectError::Invalid {
                    field: "defaults",
                    ..
                }))
            ),
            "{refused:?}"
        );
    }
    let updated = fixture
        .engine
        .update_project(with_agent(selection.agent.as_str(), None))
        .await
        .unwrap();
    let defaults = updated.defaults.unwrap();
    assert_eq!(defaults.agent.as_ref(), Some(&selection.agent));
    assert_eq!(defaults.preset, None);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_delegated_session_belongs_to_its_roots_project_and_cannot_move() {
    let (fixture, selection) = custom_fixture();
    let web = create(&fixture, "web", None).await;
    let root = session_in(
        &fixture,
        &selection,
        SessionRuntimes::default(),
        Some(web.id.clone()),
    )
    .await
    .unwrap();
    let child = create_buffered_delegated_child(&fixture.engine, root);
    assert_eq!(
        fixture.engine.get_session(child).unwrap().project.as_ref(),
        Some(&web.id)
    );
    // The child records nothing of its own.
    assert!(
        !events(&fixture, child)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::SessionProjectSet { .. }))
    );
    assert!(matches!(
        fixture.engine.set_session_project(child, None).await,
        Err(EngineError::Project(ProjectError::RootSessionOnly { root_session_id, .. }))
            if root_session_id == root
    ));

    // The tree moves with its root.
    let shop = create(&fixture, "shop", None).await;
    let moved = fixture
        .engine
        .set_session_project(root, Some(shop.id.clone()))
        .await
        .unwrap();
    assert_eq!(moved.project.as_ref(), Some(&shop.id));
    assert_eq!(
        fixture.engine.get_session(child).unwrap().project.as_ref(),
        Some(&shop.id)
    );
    // Moving where it already is records nothing.
    let recorded = |fixture: &Fixture| {
        events(fixture, root)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::SessionProjectSet { .. }))
            .count()
    };
    let before = recorded(&fixture);
    fixture
        .engine
        .set_session_project(root, Some(shop.id.clone()))
        .await
        .unwrap();
    assert_eq!(recorded(&fixture), before);
    // An unknown project is refused.
    assert!(matches!(
        fixture
            .engine
            .set_session_project(root, Some(ProjectId::new("0000-unknown").unwrap()))
            .await,
        Err(EngineError::Project(ProjectError::NotFound(_)))
    ));
    let moved = fixture
        .engine
        .set_session_project(root, None)
        .await
        .unwrap();
    assert_eq!(moved.project, None);
    assert_eq!(fixture.engine.get_session(child).unwrap().project, None);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn moving_a_running_session_takes_effect_at_its_next_root_admission() {
    let (endpoint, responses, captured) = scripted_channel_server(5).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent());
    let web = create(&fixture, "web", None).await;
    user_write(
        &fixture,
        "project://web/overview",
        "Mine",
        "A web shop in Rust.\n",
    )
    .await;
    user_write(
        &fixture,
        "project://web/release",
        "How releases are cut",
        "- tag, then publish\n",
    )
    .await;
    let session = session_in(&fixture, &selection, SessionRuntimes::default(), None)
        .await
        .unwrap();

    // The move lands while the first run is still waiting for the model.
    start(&fixture, session, &selection, "first", "first").await;
    wait_running(&fixture, session).await;
    let moved = fixture
        .engine
        .set_session_project(session, Some(web.id.clone()))
        .await
        .unwrap();
    assert_eq!(moved.project.as_ref(), Some(&web.id));
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "first",
            scripted_text_body("done"),
        ))
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session).await;
    for input in ["second", "third", "fourth", "fifth"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    run(&fixture, session, &selection, "second", "second").await;

    // A rename takes a new snapshot; a run with nothing changed takes none.
    let current = project(&fixture, &web.id).await;
    let mut rename = ProjectUpdateParams::new(web.id.clone(), current.revision);
    rename.name = Some(name("shop"));
    fixture.engine.update_project(rename).await.unwrap();
    run(&fixture, session, &selection, "third", "third").await;
    run(&fixture, session, &selection, "fourth", "fourth").await;
    // Moving out takes one too.
    fixture
        .engine
        .set_session_project(session, None)
        .await
        .unwrap();
    run(&fixture, session, &selection, "fifth", "fifth").await;
    let requests = captured.await.unwrap();

    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 4, "{taken:#?}");
    assert_eq!(taken[0].project, None);
    let in_web = taken[1].project.as_ref().unwrap();
    assert_eq!(in_web.project.as_str(), "web");
    let overview = in_web.overview.as_ref().unwrap();
    assert_eq!(overview.body, "A web shop in Rust.\n");
    assert_eq!(
        overview.description,
        "Overview of the `web` project, loaded in full into its sessions"
    );
    assert_eq!(
        in_web
            .memories
            .iter()
            .map(|memory| memory.url.as_str())
            .collect::<Vec<_>>(),
        ["project://web/release"]
    );
    assert_eq!(taken[2].project.as_ref().unwrap().project.as_str(), "shop");
    assert_eq!(
        taken[2].project.as_ref().unwrap().memories[0].url,
        "project://shop/release"
    );
    assert_eq!(taken[3].project, None);

    // What the model read.
    assert!(!requests[0].contains("## Project"));
    assert!(!requests[0].contains("Project: "));
    assert!(requests[1].contains("## Project web"));
    assert!(requests[1].contains("This session belongs to the web project."));
    assert!(requests[1].contains("A web shop in Rust."));
    // The system prompt names the project too (memory spec section 16.9.2).
    assert!(requests[1].contains("Project: web"));
    assert!(requests[2].contains("Project: shop"));
    assert!(!requests[4].contains("Project: "));
    assert!(requests[1].contains("- project://web/release: How releases are cut"));
    assert!(requests[2].contains("## Project shop"));
    assert!(!requests[4].contains("## Project"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_projects_agents_md_comes_first_in_the_agents_md_turn() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent());
    write_private_test_file(
        &fixture._directory.path().join("AGENTS.md"),
        "workspace rules",
    );
    let web = create(&fixture, "web", None).await;
    let mut params = ProjectUpdateParams::new(web.id.clone(), web.revision.clone());
    params.agents_md = Some("project rules".into());
    let updated = fixture.engine.update_project(params).await.unwrap();
    assert!(updated.has_agents_md);
    assert_ne!(updated.revision, web.revision);
    let session = session_in(
        &fixture,
        &selection,
        SessionRuntimes::default(),
        Some(web.id.clone()),
    )
    .await
    .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "go",
            scripted_text_body("done"),
        ))
        .unwrap();
    run(&fixture, session, &selection, "go", "go").await;
    let requests = captured.await.unwrap();

    let loaded = events(&fixture, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AgentMdLoaded {
                entries,
                attachment,
                project,
            } => Some((attachment, project, entries)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(loaded.len(), 2, "{loaded:?}");
    let (attachment, project, entries) = &loaded[0];
    assert_eq!(attachment.as_str(), "global");
    let project = project.as_ref().unwrap();
    assert_eq!((&project.id, project.name.as_str()), (&web.id, "web"));
    assert_eq!(entries[0].source.as_str(), "project:web/AGENTS.md");
    assert_eq!(entries[0].content, "project rules");
    assert!(loaded[1].1.is_none());

    // General before specific: the project's file, then the workspace's.
    let request = &requests[0];
    let project_at = request
        .find("<contents from=\\\"project:web/AGENTS.md\\\">")
        .unwrap();
    let workspace_at = request.find("workspace rules").unwrap();
    assert!(project_at < workspace_at, "{request}");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn deleting_a_project_refuses_while_a_session_runs_then_deletes_sessions_and_memories() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent());
    let web = create(&fixture, "web", None).await;
    user_write(&fixture, "project://web/notes", "Notes", "- one\n").await;
    let folder = fixture._directory.path().join("data/memory/projects/web");
    std::fs::write(folder.join("scratch.txt"), "not a memory").unwrap();
    let busy = session_in(
        &fixture,
        &selection,
        SessionRuntimes::default(),
        Some(web.id.clone()),
    )
    .await
    .unwrap();
    let idle = session_in(
        &fixture,
        &selection,
        SessionRuntimes::default(),
        Some(web.id.clone()),
    )
    .await
    .unwrap();
    let outside = session_in(&fixture, &selection, SessionRuntimes::default(), None)
        .await
        .unwrap();
    let revision = project(&fixture, &web.id).await.revision;

    start(&fixture, busy, &selection, "busy", "busy").await;
    wait_running(&fixture, busy).await;
    let refused = fixture
        .engine
        .delete_project(ProjectDeleteParams {
            project: web.id.clone(),
            revision: revision.clone(),
        })
        .await;
    assert!(
        matches!(
            &refused,
            Err(EngineError::Project(ProjectError::SessionsRunning { sessions, .. }))
                if *sessions == [busy]
        ),
        "{refused:?}"
    );
    assert!(fixture.engine.get_session(idle).is_ok());
    assert!(folder.is_dir());
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "busy",
            scripted_text_body("done"),
        ))
        .unwrap();
    wait_for_session_not_running(&fixture.engine, busy).await;

    // A stale revision is refused, changing nothing.
    assert!(matches!(
        fixture
            .engine
            .delete_project(ProjectDeleteParams {
                project: web.id.clone(),
                revision: "0000000000000000".into(),
            })
            .await,
        Err(EngineError::Project(ProjectError::Changed { .. }))
    ));
    let deleted = fixture
        .engine
        .delete_project(ProjectDeleteParams {
            project: web.id.clone(),
            revision,
        })
        .await
        .unwrap();
    assert_eq!(deleted.deleted_sessions, 2);
    // The overview and the notes; the scratch file goes too.
    assert_eq!(deleted.deleted_memories, 2);
    assert!(!folder.exists());
    assert!(fixture.engine.list_projects().await.unwrap().is_empty());
    let listed = fixture
        .engine
        .list_sessions()
        .into_iter()
        .map(|meta| meta.session_id)
        .collect::<Vec<_>>();
    assert_eq!(listed, [outside]);
    assert!(fixture.engine.get_session(busy).is_err());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn memory_writes_to_a_project_that_does_not_exist_are_refused() {
    let (fixture, _) = custom_fixture();
    let refused = fixture
        .engine
        .write_memory(MemoryWriteParams {
            url: "project://ghost/notes".into(),
            text: "---\ndescription: Notes\n---\n".into(),
            revision: None,
        })
        .await;
    let Err(EngineError::MemoryRequest(crate::MemoryRequestError::Refused { reason, .. })) =
        refused
    else {
        panic!("{refused:?}");
    };
    assert!(reason.contains("There are no projects yet"), "{reason}");
    create(&fixture, "ghost", None).await;
    fixture
        .engine
        .write_memory(MemoryWriteParams {
            url: "project://ghost/notes".into(),
            text: "---\ndescription: Notes\n---\n".into(),
            revision: None,
        })
        .await
        .unwrap();
    fixture.engine.shutdown().await;
}
