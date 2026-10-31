//! `cookie daemon`: its token and the lines it prints.

use super::*;

#[test]
fn the_daemon_keeps_a_fresh_token_in_its_config_and_reuses_a_configured_one() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "# mine\n").unwrap();
    let created = daemon_token(None, Some(&path)).expect("create a token");
    let reloaded = cookie_agent_config::load_from_user_root(Some(directory.path()))
        .expect("reload the configuration");
    assert_eq!(
        reloaded.runtime.server.token.as_deref(),
        Some(created.as_str())
    );
    assert!(std::fs::read_to_string(&path).unwrap().contains("# mine"));
    let reused = daemon_token(Some(TEST_TOKEN.into()), Some(&path)).unwrap();
    assert_eq!(reused.as_str(), TEST_TOKEN);
    assert!(daemon_token(None, None).is_err());
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
