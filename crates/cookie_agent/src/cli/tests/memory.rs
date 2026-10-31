//! `cookie memory`.

use super::*;

#[test]
fn memory_commands_take_a_layer_a_url_and_a_confirmation_switch() {
    use crate::cli::memory::MemoryCommand;
    use cookie_agent_protocol::MemoryLayer;
    let parsed = |arguments: &[&str]| match Cli::try_parse_from(arguments).unwrap().command {
        Some(Command::Memory { command, .. }) => command,
        other => panic!("not a memory command: {other:?}"),
    };
    assert_eq!(
        parsed(&["cookie", "memory", "list"]),
        MemoryCommand::List { layer: None }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "list", "project"]),
        MemoryCommand::List {
            layer: Some(MemoryLayer::Project)
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "show", "project://web/build"]),
        MemoryCommand::Show {
            url: "project://web/build".into()
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "edit", "project://web/build"]),
        MemoryCommand::Edit {
            url: "project://web/build".into()
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "rm", "user://hardware", "--yes"]),
        MemoryCommand::Rm {
            url: "user://hardware".into(),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "rm", "user://hardware"]),
        MemoryCommand::Rm {
            url: "user://hardware".into(),
            yes: false
        }
    );
    for arguments in [
        &["cookie", "memory", "list", "projects"][..],
        &["cookie", "memory", "edit"][..],
        &["cookie", "memory", "show"][..],
        &["cookie", "memory", "rm"][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

/// `cookie memory` lists, creates, edits, and deletes a running daemon's
/// memories through `memory.*`: invalid text is offered again as it was
/// left, a memory changed meanwhile is reloaded, and `rm` asks first.
#[tokio::test]
async fn cookie_memory_lists_edits_and_deletes_the_daemon_memories() {
    use crate::cli::memory::{MemoryCommand, NEW_MEMORY, run_memory_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_empty_config(&workspace);
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
    let memory = temporary.path().join("data/memory");
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    // A project memory needs its project.
    client
        .create_project(cookie_agent_protocol::ProjectCreateParams {
            name: cookie_agent_protocol::ProjectName::new("web").unwrap(),
            emoji: None,
            defaults: None,
        })
        .await
        .unwrap();
    let url = "project://web/build";
    let valid = "---\ndescription: Build conventions\n---\nRun the gates.\n";

    // A new memory starts from the template; invalid text is offered again
    // as it was left.
    let mut io = ScriptedMemoryIo::new(&["---\ndescription: \n---\n", valid], &[true]);
    run_memory_with(&client, MemoryCommand::Edit { url: url.into() }, &mut io)
        .await
        .expect("create");
    assert_eq!(io.seen, [NEW_MEMORY, "---\ndescription: \n---\n"]);
    assert!(
        io.output[0].starts_with("Not saved: The text is not a valid memory"),
        "{:?}",
        io.output
    );
    assert_eq!(io.output.last().unwrap(), "Created project://web/build.");
    assert_eq!(
        unstamped(&std::fs::read_to_string(memory.join("projects/web/build.md")).unwrap()),
        valid
    );

    // Leaving a new memory's template as it is creates nothing.
    let mut io = ScriptedMemoryIo::new(&[NEW_MEMORY], &[]);
    run_memory_with(
        &client,
        MemoryCommand::Edit {
            url: "project://web/empty".into(),
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(
        io.output,
        ["No changes; project://web/empty was not created."]
    );

    // A memory changed while it was open is reloaded and edited again.
    let changed = "---\ndescription: Build conventions\n---\nRun every gate.\n";
    let mine = "---\ndescription: Build conventions\n---\nMine.\n";
    let mut io = ScriptedMemoryIo::new(&[mine, mine], &[true]);
    io.meanwhile = Some((memory.join("projects/web/build.md"), changed.to_owned()));
    run_memory_with(&client, MemoryCommand::Edit { url: url.into() }, &mut io)
        .await
        .expect("edit");
    assert_eq!(
        io.seen
            .iter()
            .map(|text| unstamped(text))
            .collect::<Vec<_>>(),
        [valid, changed]
    );
    assert_eq!(
        io.output,
        [
            "Not saved: project://web/build changed since it was opened.",
            "Saved project://web/build."
        ]
    );
    assert_eq!(
        unstamped(&std::fs::read_to_string(memory.join("projects/web/build.md")).unwrap()),
        mine
    );

    // The listing groups by layer and project, gives each memory's stamps
    // and its size against its limit, and names what is not a memory.
    std::fs::write(memory.join("projects/web/notes.txt"), "stray").unwrap();
    // Sizes count the whole file, the daemon's stamps included.
    let size = |path: &str| std::fs::metadata(memory.join(path)).unwrap().len();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(
        &client,
        MemoryCommand::List {
            layer: Some(cookie_agent_protocol::MemoryLayer::Project),
        },
        &mut io,
    )
    .await
    .unwrap();
    // The limit is the configuration's; only its place is checked.
    let stamps = |line: &str, path: &str| {
        line.starts_with(&format!(
            "      created less than a minute ago · modified less than a minute ago · {} bytes of ",
            size(path)
        )) && line.ends_with(" KB")
    };
    assert_eq!(
        io.output[..3],
        [
            "Projects",
            "  web",
            "    project://web/build  Build conventions"
        ]
    );
    assert!(
        stamps(&io.output[3], "projects/web/build.md"),
        "{:?}",
        io.output
    );
    assert_eq!(
        io.output[4],
        "    project://web/overview  Overview of the `web` project, loaded in full into its sessions"
    );
    assert!(
        stamps(&io.output[5], "projects/web/overview.md"),
        "{:?}",
        io.output
    );
    assert_eq!(
        io.output[6..],
        [
            "Not memories (ignored)",
            "  projects/web/notes.txt: not a `.md` file"
        ]
    );

    // `show` gives the stamps with their exact times, the size against the
    // limit, and the text without the stamp lines.
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(&client, MemoryCommand::Show { url: url.into() }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], url);
    assert!(
        io.output[1].starts_with("created: ")
            && io.output[1].ends_with(" UTC (less than a minute ago)"),
        "{:?}",
        io.output
    );
    assert!(io.output[2].starts_with("modified: "), "{:?}", io.output);
    assert!(
        io.output[3].starts_with(&format!(
            "size: {} bytes of ",
            size("projects/web/build.md")
        )),
        "{:?}",
        io.output
    );
    assert!(io.output[4].starts_with("revision: "), "{:?}", io.output);
    assert_eq!(io.output[5..], ["", mine.trim_end()]);
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(&client, MemoryCommand::List { layer: None }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], "User");
    assert!(io.output[1].starts_with("  user://feedback  Corrections"));

    // `rm` asks first; declining keeps the memory.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_memory_with(
        &client,
        MemoryCommand::Rm {
            url: url.into(),
            yes: false,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output, ["project://web/build was not deleted."]);
    assert!(memory.join("projects/web/build.md").exists());
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(
        &client,
        MemoryCommand::Rm {
            url: url.into(),
            yes: true,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output, ["Deleted project://web/build."]);
    assert!(!memory.join("projects/web/build.md").exists());

    // A refusal is the daemon's error.
    let refused = run_memory_with(
        &client,
        MemoryCommand::Rm {
            url: url.into(),
            yes: true,
        },
        &mut ScriptedMemoryIo::new(&[], &[]),
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .chain()
            .filter_map(|cause| cause.downcast_ref::<cookie_agent_protocol::ClientError>())
            .any(|error| error.is(&cookie_agent_protocol::errors::MEMORY_NOT_FOUND)),
        "{refused:#}"
    );
    // It reads as the message and its hint, the hint once.
    let shown = format!("{refused:#}");
    let hint = cookie_agent_protocol::errors::MEMORY_NOT_FOUND
        .hint
        .expect("memory_not_found has a hint");
    assert_eq!(shown.matches(hint).count(), 1, "{shown}");
    assert!(shown.starts_with("There is no memory at "), "{shown}");

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
