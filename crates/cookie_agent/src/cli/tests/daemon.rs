//! `cookie daemon`: its tokens and the lines it prints.

use super::*;

#[test]
fn the_daemon_keeps_fresh_tokens_in_its_config_and_reuses_configured_ones() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "# mine\n").unwrap();
    let unset = cookie_agent_config::ServerConfig::default();
    let created = daemon_tokens(&unset, Some(&path)).expect("create the tokens");
    assert!(created.token_created && created.runtime_token_created);
    assert_ne!(created.token.as_str(), created.runtime_token.as_str());
    let reloaded = cookie_agent_config::load_from_user_root(Some(directory.path()))
        .expect("reload the configuration");
    let server = &reloaded.runtime.server;
    assert_eq!(server.token.as_deref(), Some(created.token.as_str()));
    assert_eq!(
        server.runtime_token.as_deref(),
        Some(created.runtime_token.as_str())
    );
    assert!(std::fs::read_to_string(&path).unwrap().contains("# mine"));
    let reused = daemon_tokens(server, Some(&path)).unwrap();
    assert!(!reused.token_created && !reused.runtime_token_created);
    assert_eq!(reused.token.as_str(), created.token.as_str());
    assert_eq!(
        reused.runtime_token.as_str(),
        created.runtime_token.as_str()
    );
    assert!(daemon_tokens(&unset, None).is_err());
}

/// A configuration from before the runtime token gains one and keeps its
/// client token.
#[test]
fn a_daemon_with_only_a_client_token_adds_a_runtime_token() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, format!("[server]\ntoken = \"{TEST_TOKEN}\"\n")).unwrap();
    let loaded = cookie_agent_config::load_from_user_root(Some(directory.path())).unwrap();
    let tokens = daemon_tokens(&loaded.runtime.server, Some(&path)).unwrap();
    assert!(!tokens.token_created && tokens.runtime_token_created);
    assert_eq!(tokens.token.as_str(), TEST_TOKEN);
    let reloaded = cookie_agent_config::load_from_user_root(Some(directory.path())).unwrap();
    assert_eq!(
        reloaded.runtime.server.runtime_token.as_deref(),
        Some(tokens.runtime_token.as_str())
    );
}

#[test]
fn the_daemon_names_its_page() {
    assert_eq!(
        web_page_line("ws://127.0.0.1:7419/ws").as_deref(),
        Some("web page at http://127.0.0.1:7419/ (`cookie web` prints it with the token)")
    );
    let line = web_page_line("wss://lab.example:8443/ws").expect("a page");
    assert!(
        line.starts_with("web page at https://lab.example:8443/ "),
        "{line}"
    );
    // The line carries no token.
    assert!(!line.contains("token="), "{line}");
}

fn starting_task(id: &str, label: &str) -> cookie_agent_protocol::DaemonTask {
    cookie_agent_protocol::DaemonTask {
        id: id.into(),
        label: label.into(),
        detail: None,
        progress: None,
        blocking: true,
        started_at: jiff::Timestamp::UNIX_EPOCH,
    }
}

#[test]
fn startup_lines_follow_the_task_and_hold_back_repeats() {
    let mut lines = crate::cli::StartupLines::default();
    let mut status = cookie_agent_protocol::DaemonStatus {
        phase: cookie_agent_protocol::DaemonPhase::Starting,
        tasks: vec![starting_task("1", "Loading the catalog")],
        ..cookie_agent_protocol::DaemonStatus::default()
    };
    assert!(lines.next(&status).is_some());
    assert!(lines.next(&status).is_none(), "the same task, too soon");
    status
        .tasks
        .push(starting_task("2", "Opening the session store"));
    assert_eq!(
        lines.next(&status).as_deref(),
        Some("Cookie is starting: Opening the session store")
    );
}
