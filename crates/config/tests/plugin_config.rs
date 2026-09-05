use std::fs;

use cookie_agent_config::load_from_roots;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn enabled_plugin_requires_command() {
    let directory = root("[plugins.demo]\n");
    let error = load_from_roots(Some(directory.path()), None).expect_err("missing command");
    assert!(error.to_string().contains("invalid field `command`"));
}

#[test]
fn producer_messaging_defaults_off_and_accepts_boolean_opt_in() {
    let defaults = root("[plugins.demo]\ncommand = \"plugin\"\n");
    let loaded = load_from_roots(Some(defaults.path()), None).expect("default capability");
    assert!(!loaded.plugins["demo"].producer_messaging);

    for (configured, expected) in [("true", true), ("false", false)] {
        let directory = root(&format!(
            "[plugins.demo]\ncommand = \"plugin\"\nproducer_messaging = {configured}\n"
        ));
        let loaded = load_from_roots(Some(directory.path()), None).expect("boolean capability");
        assert_eq!(loaded.plugins["demo"].producer_messaging, expected);
    }
}

#[test]
fn plugin_fields_are_strict() {
    let directory = root("[plugins.demo]\ncommand = \"plugin\"\nunknown = true\n");
    let error = load_from_roots(Some(directory.path()), None).expect_err("unknown field");
    assert!(error.to_string().contains("unknown"));

    for value in ["\"true\"", "1", "[]", "{}"] {
        let directory = root(&format!(
            "[plugins.demo]\ncommand = \"plugin\"\nproducer_messaging = {value}\n"
        ));
        let error = load_from_roots(Some(directory.path()), None).expect_err("non-boolean field");
        assert!(
            error
                .to_string()
                .contains("malformed configuration content")
        );
    }
}

#[test]
fn plugin_timeouts_must_be_positive() {
    for timeout in [
        "interception_timeout_ms = 0",
        "startup_timeout_ms = 0",
        "shutdown_grace_ms = 0",
        "tool_timeout_ms = 0",
    ] {
        let directory = root(&format!(
            "[plugins.demo]\ncommand = \"plugin\"\n{timeout}\n"
        ));
        let error = load_from_roots(Some(directory.path()), None).expect_err("invalid timeout");
        assert!(error.to_string().contains("invalid field"));
        assert!(
            error
                .to_string()
                .contains(timeout.split_once(' ').unwrap().0)
        );
        assert!(error.to_string().contains("line 3"));
    }
}

#[test]
fn disabled_plugin_with_valid_fields_is_accepted() {
    let directory = root("[plugins.demo]\ncommand = \"plugin\"\nenabled = false\n");
    let loaded = load_from_roots(Some(directory.path()), None).expect("disabled plugin");
    assert!(!loaded.plugins["demo"].enabled);
}

#[test]
fn disabled_plugin_still_rejects_invalid_fields() {
    for invalid in ["cwd = \"\"", "startup_timeout_ms = 0"] {
        let directory = root(&format!(
            "[plugins.demo]\ncommand = \"plugin\"\nenabled = false\n{invalid}\n"
        ));
        let error = load_from_roots(Some(directory.path()), None).expect_err("invalid disabled");
        assert!(error.to_string().contains("invalid field"));
        assert!(error.to_string().contains("line 4"));
    }
}

#[test]
fn workspace_plugin_overrides_user_entry_by_name() {
    let user = root(
        "[plugins.zeta]\ncommand = \"zeta\"\n[plugins.demo]\ncommand = \"user-plugin\"\nproducer_messaging = true\n[plugins.alpha]\ncommand = \"alpha\"\n",
    );
    let workspace = root("[plugins.demo]\ncommand = \"workspace-plugin\"\n");
    let loaded = load_from_roots(Some(user.path()), Some(workspace.path())).expect("plugins");
    assert_eq!(
        loaded.plugins["demo"].command.as_deref(),
        Some("workspace-plugin")
    );
    assert!(!loaded.plugins["demo"].producer_messaging);
    assert_eq!(
        loaded
            .plugins
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["zeta", "demo", "alpha"]
    );
}
