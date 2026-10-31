//! `cookie cron`.

use super::*;

#[test]
fn cron_commands_take_ids_targets_and_options() {
    use crate::cli::cron::{CronCommand, parse_job_id};
    let parsed = |arguments: &[&str]| match Cli::try_parse_from(arguments).unwrap().command {
        Some(Command::Cron { command, .. }) => command,
        other => panic!("not a cron command: {other:?}"),
    };
    let id = parse_job_id("k3v9q2xa").unwrap();
    assert_eq!(parsed(&["cookie", "cron", "list"]), CronCommand::List);
    assert_eq!(
        parsed(&["cookie", "cron", "show", "k3v9q2xa"]),
        CronCommand::Show { id: id.clone() }
    );
    assert_eq!(
        parsed(&["cookie", "cron", "rm", "k3v9q2xa", "-y"]),
        CronCommand::Rm {
            id: id.clone(),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["cookie", "cron", "run", "k3v9q2xa", "--url", "ws://h:1/ws"]),
        CronCommand::Run { id: id.clone() }
    );
    assert!(matches!(
        parsed(&[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--agent",
            "primary",
            "--permission-mode",
            "ask",
            "--model",
            "a/b"
        ]),
        CronCommand::Create(_)
    ));
    assert!(matches!(
        parsed(&["cookie", "cron", "edit", "k3v9q2xa", "--name", ""]),
        CronCommand::Edit { .. }
    ));
    for arguments in [
        // A job id is eight lowercase letters and digits.
        &["cookie", "cron", "show", "K3V9Q2XA"][..],
        // A target is required, and only one.
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
        ][..],
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--agent",
            "a",
            "--session",
            "0190a5b2-0000-7000-8000-000000000001",
        ][..],
        // A new-session option needs a new-session job, delivery a session.
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--session",
            "0190a5b2-0000-7000-8000-000000000001",
            "--permission-mode",
            "ask",
        ][..],
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--agent",
            "a",
            "--delivery",
            "steer",
        ][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

/// `cookie cron` creates, lists, shows, edits, disables, enables, runs, and
/// deletes a running daemon's cron jobs through `crons.*`.
#[tokio::test]
async fn cookie_cron_manages_the_daemon_jobs() {
    use crate::cli::cron::{CronCommand, parse_job_id, run_cron_with};
    use clap::Parser as _;
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
    let command = |arguments: &[&str]| {
        let mut full = vec!["cookie", "cron"];
        full.extend_from_slice(arguments);
        match Cli::try_parse_from(full).unwrap().command {
            Some(Command::Cron { command, .. }) => command,
            other => panic!("not a cron command: {other:?}"),
        }
    };

    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, CronCommand::List, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output, ["No cron jobs."]);

    // A new-session job with a mode only a client sets.
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(
        &client,
        command(&[
            "create",
            "--name",
            "Nightly",
            "--prompt",
            "Check the dependencies.",
            "--schedule",
            "0 3 * * *",
            "--max-runs",
            "-1",
            "--agent",
            "primary",
            "--permission-mode",
            "ask",
        ]),
        &mut io,
    )
    .await
    .unwrap();
    let created = io.output[0].clone();
    assert!(
        created.starts_with("Created job ") && created.ends_with(" (Nightly)."),
        "{created}"
    );
    assert!(io.output[1].starts_with("Next fire: "), "{:?}", io.output);
    let jobs = client.list_crons().await.unwrap().jobs;
    assert_eq!(jobs.len(), 1);
    let id = jobs[0].id.clone();
    assert_eq!(
        jobs[0].permission_mode,
        Some(cookie_agent_protocol::PermissionMode::Ask)
    );

    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, CronCommand::List, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], format!("{id}  Nightly"));
    assert!(
        io.output[1].starts_with("    every day at 03:00 · next ")
            && io.output[1].ends_with(" · new session · primary · last never"),
        "{:?}",
        io.output
    );

    // An in-session job, edited: only what changed is sent.
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
    let session_text = session.to_string();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(
        &client,
        command(&[
            "create",
            "--prompt",
            "Summarize the build log.",
            "--schedule",
            "in 2h",
            "--session",
            &session_text,
            "--delivery",
            "steer",
        ]),
        &mut io,
    )
    .await
    .unwrap();
    let jobs = client.list_crons().await.unwrap().jobs;
    let second = jobs[1].id.clone();
    assert_eq!(
        jobs[1].target,
        cookie_agent_protocol::CronTarget::Session(session)
    );
    assert_eq!(
        jobs[1].delivery,
        Some(cookie_agent_protocol::ProducerDeliveryMode::Steer)
    );
    let second_text = second.to_string();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(
        &client,
        command(&[
            "edit",
            &second_text,
            "--name",
            "Build log",
            "--delivery",
            "queue",
        ]),
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output[0], format!("Updated job {second} (Build log)."));
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["edit", &second_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(io.output, [format!("Job {second} is unchanged.")]);
    let refused = run_cron_with(
        &client,
        command(&["edit", &second_text, "--permission-mode", "yolo"]),
        &mut io,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{refused:#}").contains("--permission-mode is for a job that starts a new session"),
        "{refused:#}"
    );

    // Disable, show, enable.
    let id_text = id.to_string();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["disable", &id_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(
        io.output,
        [
            format!("Disabled job {id} (Nightly)."),
            "No next fire: disabled by you.".to_owned()
        ]
    );
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["show", &id_text]), &mut io)
        .await
        .unwrap();
    for expected in [
        "Nightly",
        "schedule: 0 3 * * *",
        "reads: every day at 03:00",
        "target: new session · primary",
        "state: disabled by you",
        "runs: 0, no limit",
        "mode: ask",
        "Check the dependencies.",
        "No fires yet.",
    ] {
        assert!(
            io.output.iter().any(|line| line == expected),
            "{expected}: {:?}",
            io.output
        );
    }
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["enable", &id_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], format!("Enabled job {id} (Nightly)."));
    assert!(io.output[1].starts_with("Next fire: "), "{:?}", io.output);

    // Run now records a run now in the history.
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["run", &second_text]), &mut io)
        .await
        .unwrap();
    assert!(
        io.output[0].starts_with(&format!("Ran job {second}: run now · ")),
        "{:?}",
        io.output
    );

    // Delete asks first.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_cron_with(&client, command(&["rm", &id_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(io.output, [format!("Job {id} was not deleted.")]);
    run_cron_with(&client, command(&["rm", &id_text, "--yes"]), &mut io)
        .await
        .unwrap();
    assert_eq!(
        io.output.last().unwrap(),
        &format!("Deleted job {id} (Nightly).")
    );
    let missing = run_cron_with(
        &client,
        CronCommand::Show {
            id: parse_job_id(&id_text).unwrap(),
        },
        &mut io,
    )
    .await
    .unwrap_err();
    assert!(format!("{missing:#}").contains(&format!("there is no cron job {id}")));

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
