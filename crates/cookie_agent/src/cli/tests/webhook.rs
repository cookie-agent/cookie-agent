//! `cookie webhook`.

use super::*;

fn parsed(arguments: &[&str]) -> crate::cli::webhook::WebhookCommand {
    let mut full = vec!["cookie", "webhook"];
    full.extend_from_slice(arguments);
    match Cli::try_parse_from(full).unwrap().command {
        Some(Command::Webhook { command, .. }) => command,
        other => panic!("not a webhook command: {other:?}"),
    }
}

#[test]
fn webhook_commands_take_ids_targets_and_options() {
    use crate::cli::webhook::{RotateArg, WebhookCommand, parse_webhook_id};
    let id = parse_webhook_id("w7k2m9qa").unwrap();
    assert_eq!(parsed(&["list"]), WebhookCommand::List);
    assert_eq!(
        parsed(&["show", "w7k2m9qa", "--url", "ws://h:1/ws"]),
        WebhookCommand::Show { id: id.clone() }
    );
    assert_eq!(
        parsed(&["rm", "w7k2m9qa", "-y"]),
        WebhookCommand::Rm {
            id: id.clone(),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["rotate", "w7k2m9qa", "secret", "--url", "ws://h:1/ws"]),
        WebhookCommand::Rotate {
            id: id.clone(),
            what: RotateArg::Secret,
        }
    );
    let WebhookCommand::Test { headers, body, .. } = parsed(&[
        "test",
        "w7k2m9qa",
        "--body",
        "-",
        "--header",
        "X-GitHub-Event: issues",
        "--header",
        "X-Empty:",
    ]) else {
        panic!("a test command");
    };
    assert_eq!(body.as_deref(), Some(std::path::Path::new("-")));
    assert_eq!(
        headers
            .iter()
            .map(|header| (header.name.as_str(), header.value.as_str()))
            .collect::<Vec<_>>(),
        [("X-GitHub-Event", "issues"), ("X-Empty", "")]
    );
    assert!(matches!(
        parsed(&[
            "create",
            "--agent",
            "primary",
            "--thread-key",
            "json:/issue/number",
            "--thread-prompt",
            "Look after one issue.",
            "--delivery",
            "steer",
            "--check",
            "hmac_sha256 header=X-Sig encoding=hex",
            "--secret",
            "-s3cret",
            "--filter",
            "json:/action=opened",
            "--filter",
            "header:X-GitHub-Event=issues",
            "--show-header",
            "X-GitHub-Event",
            "--max-per-hour",
            "-1",
            "--max-concurrent",
            "2",
        ]),
        WebhookCommand::Create(_)
    ));
    assert!(matches!(
        parsed(&["edit", "w7k2m9qa", "--no-filter", "--name", ""]),
        WebhookCommand::Edit { .. }
    ));
    let session = "0190a5b2-0000-7000-8000-000000000001";
    for arguments in [
        // An id is eight lowercase letters and digits.
        &["cookie", "webhook", "show", "W7K2M9QA"][..],
        // A target is required, and only one.
        &["cookie", "webhook", "create"][..],
        &[
            "cookie",
            "webhook",
            "create",
            "--agent",
            "a",
            "--session",
            session,
        ][..],
        // New-session options need a new-session target.
        &[
            "cookie",
            "webhook",
            "create",
            "--session",
            session,
            "--thread-key",
            "json:/n",
        ][..],
        &[
            "cookie",
            "webhook",
            "create",
            "--session",
            session,
            "--max-concurrent",
            "2",
        ][..],
        // Delivery needs a session or a thread key.
        &[
            "cookie",
            "webhook",
            "create",
            "--agent",
            "a",
            "--delivery",
            "steer",
        ][..],
        // A secret needs a check.
        &[
            "cookie", "webhook", "create", "--agent", "a", "--secret", "x",
        ][..],
        // Rotate one thing.
        &["cookie", "webhook", "rotate", "w7k2m9qa"][..],
        &["cookie", "webhook", "rotate", "w7k2m9qa", "token"][..],
        &["cookie", "webhook", "rotate", "w7k2m9qa", "url", "secret"][..],
        // A header is `Name: value`.
        &[
            "cookie", "webhook", "test", "w7k2m9qa", "--header", "nocolon",
        ][..],
        &[
            "cookie",
            "webhook",
            "edit",
            "w7k2m9qa",
            "--filter",
            "json:/a=b",
            "--no-filter",
        ][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

/// `cookie webhook` creates, lists, shows, edits, disables, enables,
/// rotates, tests, and deletes a running daemon's webhooks through
/// `webhooks.*`.
#[tokio::test]
async fn cookie_webhook_manages_the_daemon_webhooks() {
    use crate::cli::webhook::run_webhook_with;
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
    let page = "http://127.0.0.1:7419/";
    let run = async |arguments: &[&str]| {
        let mut io = ScriptedMemoryIo::new(&[], &[]);
        run_webhook_with(&client, page, parsed(arguments), &mut io)
            .await
            .map(|()| io.output)
    };

    assert_eq!(run(&["list"]).await.unwrap(), ["No webhooks."]);

    let created = run(&[
        "create",
        "--name",
        "Issues",
        "--agent",
        "primary",
        "--thread-key",
        "json:/issue/number",
        "--thread-prompt",
        "Look after one issue.",
        "--check",
        "github",
        "--filter",
        "header:X-GitHub-Event=issues",
        "--permission-mode",
        "ask",
        "--disabled",
    ])
    .await
    .unwrap();
    let hooks = client.list_webhooks().await.unwrap().webhooks;
    assert_eq!(hooks.len(), 1);
    let hook = &hooks[0];
    let id = hook.id.to_string();
    assert!(!hook.enabled);
    assert_eq!(
        hook.permission_mode,
        Some(cookie_agent_protocol::PermissionMode::Ask)
    );
    assert_eq!(hook.filter.len(), 1);
    // The URL is on the page's address, and the secret is printed once.
    let url = format!("{page}{}", hook.path.trim_start_matches('/'));
    assert!(
        created.iter().any(|line| line.ends_with(&url)),
        "{created:?}"
    );
    let secret = client
        .webhook_secret(hook.id.clone())
        .await
        .unwrap()
        .secret
        .unwrap();
    assert!(
        created.iter().any(|line| line.ends_with(&secret)),
        "{created:?}"
    );
    assert_eq!(
        run(&["secret", &id]).await.unwrap(),
        std::slice::from_ref(&secret)
    );

    let listed = run(&["list"]).await.unwrap();
    assert!(listed[0].starts_with(&id), "{listed:?}");
    assert!(listed.iter().any(|line| line.trim() == url), "{listed:?}");
    let shown = run(&["show", &id]).await.unwrap();
    assert!(
        shown
            .iter()
            .any(|line| line.contains("Look after one issue."))
    );

    // An edit sends what changed; one that changes nothing sends nothing.
    run(&["edit", &id, "--name", "GitHub issues", "--no-filter"])
        .await
        .unwrap();
    let hook = client.list_webhooks().await.unwrap().webhooks.remove(0);
    assert_eq!(hook.name.as_deref(), Some("GitHub issues"));
    assert!(hook.filter.is_empty());
    assert_eq!(
        hook.permission_mode,
        Some(cookie_agent_protocol::PermissionMode::Ask)
    );
    let unchanged = run(&["edit", &id, "--name", "GitHub issues"])
        .await
        .unwrap();
    assert_eq!(unchanged.len(), 1);
    assert!(unchanged[0].contains("unchanged"), "{unchanged:?}");

    run(&["enable", &id]).await.unwrap();
    assert!(client.list_webhooks().await.unwrap().webhooks[0].enabled);
    run(&["disable", &id]).await.unwrap();
    assert!(!client.list_webhooks().await.unwrap().webhooks[0].enabled);

    // Rotating the URL changes the token; rotating the secret the secret.
    let before = client.list_webhooks().await.unwrap().webhooks.remove(0);
    run(&["rotate", &id, "url"]).await.unwrap();
    let after = client.list_webhooks().await.unwrap().webhooks.remove(0);
    assert_ne!(after.url_token, before.url_token);
    let rotated = run(&["rotate", &id, "secret"]).await.unwrap();
    let new_secret = client
        .webhook_secret(after.id.clone())
        .await
        .unwrap()
        .secret
        .unwrap();
    assert_ne!(new_secret, secret);
    assert!(rotated.iter().any(|line| line.ends_with(&new_secret)));

    // A test event is stored in the history.
    let body = temporary.path().join("body.json");
    std::fs::write(&body, r#"{"issue": {"number": 7}}"#).unwrap();
    run(&[
        "test",
        &id,
        "--body",
        body.to_str().unwrap(),
        "--content-type",
        "application/json",
        "--header",
        "X-GitHub-Event: issues",
    ])
    .await
    .unwrap();
    let hook = client.list_webhooks().await.unwrap().webhooks.remove(0);
    assert_eq!(hook.history.len(), 1);
    assert!(hook.history[0].test);

    // Deleting asks, unless told not to.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_webhook_with(&client, page, parsed(&["rm", &id]), &mut io)
        .await
        .unwrap();
    assert_eq!(client.list_webhooks().await.unwrap().webhooks.len(), 1);
    run(&["rm", &id, "--yes"]).await.unwrap();
    assert!(client.list_webhooks().await.unwrap().webhooks.is_empty());
    let missing = run(&["show", &id]).await.unwrap_err();
    assert!(format!("{missing:#}").contains(&id));

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
