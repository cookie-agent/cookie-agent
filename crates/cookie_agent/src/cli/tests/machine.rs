//! `cookie machine`.

use super::*;

#[test]
fn machine_commands_take_valid_names_and_runtime_ids() {
    use crate::cli::machine::MachineCommand;
    let name = |value: &str| cookie_agent_protocol::MachineName::new(value).unwrap();
    assert_eq!(
        Cli::try_parse_from(["cookie", "machine", "rename", "laptop", "desk-2"])
            .unwrap()
            .command,
        Some(Command::Machine {
            command: MachineCommand::Rename {
                machine: name("laptop"),
                name: name("desk-2"),
            },
            url: None,
            token: None,
        })
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "machine", "assign", "box", "labpc"])
            .unwrap()
            .command,
        Some(Command::Machine {
            command: MachineCommand::Assign {
                runtime: cookie_agent_protocol::RuntimeId::new("box").unwrap(),
                machine: name("labpc"),
            },
            url: None,
            token: None,
        })
    );
    for arguments in [
        &["cookie", "machine", "rename", "laptop", "Desk"][..],
        &["cookie", "machine", "assign", "my-box", "labpc"][..],
        &["cookie", "machine", "forget"][..],
        &["cookie", "machine", "merge", "a"][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

/// `cookie machine` lists the daemon's machines and regroups them through
/// the `machines.*` methods; a refusal is the daemon's error.
#[tokio::test]
async fn cookie_machine_lists_and_regroups_the_daemon_machines() {
    use crate::cli::machine::{MachineCommand, run_machine_with};
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
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let name = |value: &str| cookie_agent_protocol::MachineName::new(value).unwrap();
    let daemon = runtime.engine.runtime_machine("local").expect("machine");

    let listed = run_machine_with(&client, MachineCommand::List)
        .await
        .unwrap();
    assert!(
        listed[0].starts_with(&format!("{daemon} ({})", std::env::consts::OS)),
        "{listed:?}"
    );
    assert!(listed[0].ends_with(", 0 memories"), "{listed:?}");
    assert_eq!(
        listed[1],
        "  connected: global (by machine ID), local (by machine ID)"
    );

    assert_eq!(
        run_machine_with(
            &client,
            MachineCommand::Rename {
                machine: daemon.clone(),
                name: name("workstation"),
            },
        )
        .await
        .unwrap(),
        [format!("Renamed machine {daemon} to workstation.")]
    );
    run_machine_with(
        &client,
        MachineCommand::Assign {
            runtime: cookie_agent_protocol::RuntimeId::local(),
            machine: name("sandbox"),
        },
    )
    .await
    .unwrap();
    let listed = run_machine_with(&client, MachineCommand::List)
        .await
        .unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|line| !line.starts_with(' '))
            .map(|line| line.split([' ', ',']).next().unwrap())
            .collect::<Vec<_>>(),
        ["sandbox", "workstation"],
        "{listed:?}"
    );
    assert!(listed.contains(&"  connected: local (pinned)".to_owned()));
    assert!(listed.contains(&"  pinned: local".to_owned()));

    let refused = run_machine_with(
        &client,
        MachineCommand::Forget {
            machine: name("sandbox"),
        },
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .chain()
            .filter_map(|cause| cause.downcast_ref::<cookie_agent_protocol::ClientError>())
            .any(|error| error.is(&cookie_agent_protocol::errors::MACHINE_IN_USE)),
        "{refused:#}"
    );
    run_machine_with(
        &client,
        MachineCommand::Merge {
            from: name("sandbox"),
            into: name("workstation"),
        },
    )
    .await
    .unwrap();
    run_machine_with(
        &client,
        MachineCommand::Unassign {
            runtime: cookie_agent_protocol::RuntimeId::local(),
        },
    )
    .await
    .unwrap();
    let listed = run_machine_with(&client, MachineCommand::List)
        .await
        .unwrap();
    assert!(listed[0].starts_with("workstation"), "{listed:?}");
    assert_eq!(
        listed.iter().filter(|line| !line.starts_with(' ')).count(),
        1
    );

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
