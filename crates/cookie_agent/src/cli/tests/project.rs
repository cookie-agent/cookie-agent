//! `cookie project`.

use super::*;

#[test]
fn project_commands_take_names_a_session_and_a_confirmation_switch() {
    use crate::cli::project::ProjectCommand;
    let name = |value: &str| cookie_agent_protocol::ProjectName::new(value).unwrap();
    let parsed = |arguments: &[&str]| match Cli::try_parse_from(arguments).unwrap().command {
        Some(Command::Project { command, .. }) => command,
        other => panic!("not a project command: {other:?}"),
    };
    assert_eq!(parsed(&["cookie", "project", "list"]), ProjectCommand::List);
    assert_eq!(
        parsed(&["cookie", "project", "create", "web", "--emoji", "🍪"]),
        ProjectCommand::Create {
            name: name("web"),
            emoji: Some("🍪".into())
        }
    );
    assert_eq!(
        parsed(&["cookie", "project", "rename", "web", "shop"]),
        ProjectCommand::Rename {
            old: name("web"),
            new: name("shop")
        }
    );
    assert_eq!(
        parsed(&["cookie", "project", "delete", "web", "--yes"]),
        ProjectCommand::Delete {
            name: name("web"),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["cookie", "project", "edit", "web"]),
        ProjectCommand::Edit { name: name("web") }
    );
    let session = cookie_agent_protocol::SessionId::new_v7();
    assert_eq!(
        parsed(&[
            "cookie",
            "project",
            "defaults",
            "web",
            "--from-session",
            &session.to_string()
        ]),
        ProjectCommand::Defaults {
            name: name("web"),
            from_session: session
        }
    );
    for arguments in [
        &["cookie", "project", "create", "Web"][..],
        &["cookie", "project", "defaults", "web"][..],
        &["cookie", "project", "rename", "web"][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
    // `--project` names a project for plain `cookie`.
    let cli = Cli::try_parse_from(["cookie", "--project", "web"]).unwrap();
    assert_eq!(cli.project, Some(name("web")));
    assert!(cli.command.is_none());
    assert!(Cli::try_parse_from(["cookie", "--project", "Not Valid"]).is_err());
}

/// `cookie project` creates, lists, renames, edits, sets the defaults of,
/// and deletes a running daemon's projects through `project.*`.
#[tokio::test]
async fn cookie_project_manages_the_daemon_projects() {
    use crate::cli::project::{ProjectCommand, run_project_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_session_config(&workspace);
    private_directory(&cache_anchor);
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: openai_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.path().join("providers")),
        || Ok(temporary.path().join("data")),
    )
    .await
    .unwrap();
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol_transport::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let name = |value: &str| cookie_agent_protocol::ProjectName::new(value).unwrap();

    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_project_with(&client, ProjectCommand::List, &mut io)
        .await
        .unwrap();
    run_project_with(
        &client,
        ProjectCommand::Create {
            name: name("web"),
            emoji: Some("🍪".into()),
        },
        &mut io,
    )
    .await
    .unwrap();
    run_project_with(&client, ProjectCommand::List, &mut io)
        .await
        .unwrap();
    assert_eq!(
        io.output,
        [
            "No projects.",
            "Created project web.",
            "🍪 web  0 sessions, 1 memory"
        ]
    );

    // Edit writes `AGENTS.md`; an emptied file removes it.
    let mut io = ScriptedMemoryIo::new(&["Use tabs.\n"], &[]);
    run_project_with(&client, ProjectCommand::Edit { name: name("web") }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.seen, [""]);
    assert_eq!(io.output, ["Saved the AGENTS.md of project web."]);
    let mut io = ScriptedMemoryIo::new(&[""], &[]);
    run_project_with(&client, ProjectCommand::Edit { name: name("web") }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.seen, ["Use tabs.\n"]);
    assert_eq!(io.output, ["Removed the AGENTS.md of project web."]);

    // Defaults come from a session's attachments and agent.
    let selection = cookie_agent_protocol::RunSelection {
        agent: cookie_agent_protocol::AgentId::new("primary").unwrap(),
        model: cookie_agent_protocol::ModelSelection {
            model: "custom.local/test".parse().unwrap(),
            variant: None,
        },
        preset: None,
    };
    let session = client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            runtimes: Some(Vec::new()),
            ..cookie_agent_protocol::SessionCreateParams::new(selection)
        })
        .await
        .unwrap()
        .session
        .session_id;
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_project_with(
        &client,
        ProjectCommand::Defaults {
            name: name("web"),
            from_session: session,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(
        io.output,
        [
            "Set the defaults of project web.",
            "🍪 web  0 sessions, 1 memory  [agent primary]"
        ]
    );

    run_project_with(
        &client,
        ProjectCommand::Rename {
            old: name("web"),
            new: name("shop"),
        },
        &mut io,
    )
    .await
    .unwrap();
    assert!(
        io.output
            .last()
            .unwrap()
            .starts_with("Renamed project web to shop"),
        "{:?}",
        io.output
    );
    // Delete asks first.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_project_with(
        &client,
        ProjectCommand::Delete {
            name: name("shop"),
            yes: false,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output, ["Project shop was not deleted."]);
    run_project_with(
        &client,
        ProjectCommand::Delete {
            name: name("shop"),
            yes: true,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(
        io.output.last().unwrap(),
        "Deleted project shop, 0 sessions and 1 memory."
    );
    let missing = run_project_with(
        &client,
        ProjectCommand::Edit { name: name("shop") },
        &mut io,
    )
    .await
    .unwrap_err();
    assert!(format!("{missing:#}").contains("there is no project named shop"));

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
