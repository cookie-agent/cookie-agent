//! Plain `cookie`: which daemon it runs against, and the runtime it offers.

use super::*;

#[test]
fn a_tui_offers_its_machine_under_a_sanitized_host_name() {
    for (host, expected) in [
        ("laptop", "client_laptop"),
        ("My-MacBook.local", "client_my_macbook_local"),
        ("--edge__", "client_edge"),
        ("", "client"),
        (
            "a-very-long-host-name-that-goes-on-and-on",
            "client_a_very_long_host_name_tha",
        ),
    ] {
        let id = client_runtime_id(host);
        assert_eq!(id, expected, "{host}");
        cookie_agent_protocol::RuntimeId::new(id.as_str()).expect("a valid runtime ID");
    }
}

#[tokio::test]
async fn plain_cookie_connects_to_the_daemon_server_describes_and_creates_its_session_there() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    // `[server] url` names another daemon; the probe never reads it.
    let server = scratch_server(
        &temporary.path().join("home"),
        &format!(
            "port = {}\ntoken = \"{TEST_TOKEN}\"\nurl = \"ws://192.0.2.1:7419/ws\"\n",
            daemon.port()
        ),
    );
    let local_daemon::Probe::Connected(found) =
        local_daemon::probe(&server, probe_identity()).await
    else {
        panic!("the listening daemon was not found");
    };
    assert_eq!(found.url, format!("ws://127.0.0.1:{}/ws", daemon.port()));
    assert_eq!(found.token.as_str(), TEST_TOKEN);

    // The daemon runs on this machine: its `local` serves the directory.
    let choice = local_daemon::runtime_for(
        &found.client,
        &found.hello,
        paths::machine_id().as_deref(),
        "client_test",
    )
    .await;
    assert_eq!(choice, local_daemon::RuntimeChoice::Local);
    let created = found
        .client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            cwd: Some(daemon.workspace.to_string_lossy().into_owned()),
            ..cookie_agent_protocol::SessionCreateParams::new(session_selection())
        })
        .await
        .unwrap();
    let listed = found
        .client
        .list_sessions(cookie_agent_protocol::SessionListParams {
            project: None,
            cwd_identity: None,
        })
        .await
        .unwrap();
    assert!(
        listed
            .sessions
            .iter()
            .any(|session| session.session_id == created.session.session_id),
        "{listed:?}"
    );
    drop(found);
    daemon.stop().await;
}

#[tokio::test]
async fn plain_cookie_stays_in_process_silently_without_a_token_or_a_listener() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    // No token: nothing is dialed, even with a daemon at the port.
    let without_token = scratch_server(
        &temporary.path().join("without-token"),
        &format!("port = {}\n", daemon.port()),
    );
    let probe = local_daemon::probe(&without_token, probe_identity()).await;
    assert!(matches!(probe, local_daemon::Probe::NoToken));
    assert_eq!(probe.notice(), None);
    daemon.stop().await;

    // A port nothing listens on refuses the connection at once.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let nothing = scratch_server(
        &temporary.path().join("nothing"),
        &format!("port = {port}\ntoken = \"{TEST_TOKEN}\"\n"),
    );
    let started = std::time::Instant::now();
    let probe = local_daemon::probe(&nothing, probe_identity()).await;
    assert!(matches!(probe, local_daemon::Probe::NothingListens));
    assert!(started.elapsed() < local_daemon::CONNECT_DEADLINE);
    assert_eq!(probe.notice(), None);
}

#[tokio::test]
async fn a_daemon_refusing_the_token_leaves_plain_cookie_in_process_with_a_notice() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    let server = scratch_server(
        &temporary.path().join("home"),
        &format!("port = {}\ntoken = \"{OTHER_TOKEN}\"\n", daemon.port()),
    );
    let probe = local_daemon::probe(&server, probe_identity()).await;
    let local_daemon::Probe::Unusable { url, reason } = &probe else {
        panic!("a refused token must leave the daemon unusable");
    };
    assert_eq!(url, &format!("ws://127.0.0.1:{}/ws", daemon.port()));
    assert_eq!(reason, "HTTP error: 401 Unauthorized");
    let notice = probe.notice().unwrap();
    assert!(notice.contains(url.as_str()), "{notice}");
    assert!(notice.contains("401"), "{notice}");
    daemon.stop().await;
}

#[tokio::test]
async fn a_listener_that_is_no_cookie_daemon_leaves_a_notice_within_the_deadline() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    // Accepts and closes at once, as a TLS listener does with plain HTTP.
    let closing = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closing_port = closing.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = closing.accept().await {
            drop(stream);
        }
    });
    let server = scratch_server(
        &temporary.path().join("closing"),
        &format!("port = {closing_port}\ntoken = \"{TEST_TOKEN}\"\n"),
    );
    let probe = local_daemon::probe(&server, probe_identity()).await;
    assert!(
        matches!(probe, local_daemon::Probe::Unusable { .. }),
        "a listener that closes is something that answered"
    );
    assert!(probe.notice().is_some());

    // Accepts and never answers: the handshake deadline ends the wait.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    let held = Arc::new(Mutex::new(Vec::new()));
    let holder = Arc::clone(&held);
    tokio::spawn(async move {
        while let Ok((stream, _)) = silent.accept().await {
            holder.lock().unwrap().push(stream);
        }
    });
    let server = scratch_server(
        &temporary.path().join("silent"),
        &format!("port = {silent_port}\ntoken = \"{TEST_TOKEN}\"\n"),
    );
    let deadline = Duration::from_millis(200);
    let started = std::time::Instant::now();
    let probe = local_daemon::probe_within(
        &server,
        probe_identity(),
        local_daemon::CONNECT_DEADLINE,
        deadline,
    )
    .await;
    assert!(started.elapsed() >= deadline);
    let local_daemon::Probe::Unusable { reason, .. } = &probe else {
        panic!("a silent listener must leave the daemon unusable");
    };
    assert_eq!(reason, "no handshake within 200 ms");
}

#[tokio::test]
async fn a_daemon_on_another_machine_gets_a_dial_in_runtime_for_the_directory() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    let server = scratch_server(
        &temporary.path().join("home"),
        &format!("port = {}\ntoken = \"{TEST_TOKEN}\"\n", daemon.port()),
    );
    let local_daemon::Probe::Connected(found) =
        local_daemon::probe(&server, probe_identity()).await
    else {
        panic!("the listening daemon was not found");
    };
    // This client's machine is another than the daemon's, and no runtime of
    // the daemon runs on it.
    let own = "client_elsewhere";
    let choice =
        local_daemon::runtime_for(&found.client, &found.hello, Some("another-machine"), own).await;
    assert_eq!(choice, local_daemon::RuntimeChoice::DialIn);

    let directory = temporary.path().join("client-directory");
    private_directory(&directory);
    let directory = directory.canonicalize().unwrap();
    let directory = directory.to_string_lossy().into_owned();
    start_dial_in(&found.url, &found.token, own, Some(&directory)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let runtimes = found.client.list_runtimes().await.unwrap();
        if runtimes
            .runtimes
            .iter()
            .any(|runtime| runtime.runtime_id.as_str() == own && runtime.connected)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the dial-in never connected: {runtimes:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let created = found
        .client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            cwd: Some(directory.clone()),
            runtimes: Some(vec![cookie_agent_protocol::RuntimeAttachRequest {
                runtime_id: cookie_agent_protocol::RuntimeId::new(own).unwrap(),
                capability: Some(cookie_agent_protocol::CapabilityName::coding()),
                workspace: Some(directory.clone()),
            }]),
            ..cookie_agent_protocol::SessionCreateParams::new(session_selection())
        })
        .await
        .unwrap();
    assert!(
        created
            .runtimes
            .iter()
            .any(|attachment| attachment.runtime_id.as_str() == own
                && attachment.workspace.as_deref() == Some(directory.as_str())),
        "{:?}",
        created.runtimes
    );
    drop(found);
    daemon.stop().await;
}
