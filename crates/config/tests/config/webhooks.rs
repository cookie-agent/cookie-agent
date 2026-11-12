//! `[webhooks]` (webhooks spec section 11).

use std::fs;

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn loads(config: &str) -> bool {
    load_from_user_root(Some(root(config).path())).is_ok()
}

#[test]
fn every_webhooks_key_is_read() {
    let directory = root(
        "[webhooks]\nenabled = false\nmax_body_bytes = 2048\ninline_body_bytes = 512\n\
         max_per_hour = 7\nmax_consecutive_failures = 9\nbacklog = 11\nhistory = 13\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let webhooks = loaded.runtime.webhooks;
    assert!(!webhooks.enabled);
    assert_eq!(webhooks.max_body_bytes, 2048);
    assert_eq!(webhooks.inline_body_bytes, 512);
    assert_eq!(webhooks.per_hour_limit(), Some(7));
    assert_eq!(webhooks.failure_limit(), Some(9));
    assert_eq!(webhooks.backlog, 11);
    assert_eq!(webhooks.history, 13);
}

#[test]
fn zero_or_less_means_no_rate_limit_and_never_disabling() {
    for value in [0, -1] {
        let directory = root(&format!(
            "[webhooks]\nmax_per_hour = {value}\nmax_consecutive_failures = {value}\n"
        ));
        let webhooks = load_from_user_root(Some(directory.path()))
            .expect("loads")
            .runtime
            .webhooks;
        assert_eq!(webhooks.per_hour_limit(), None, "{value}");
        assert_eq!(webhooks.failure_limit(), None, "{value}");
    }
}

#[test]
fn sizes_and_counts_have_bounds() {
    for (key, low, high) in [
        ("max_body_bytes", 1, 64 * 1024 * 1024),
        ("inline_body_bytes", 1, 1024 * 1024),
        ("backlog", 1, 10_000),
        ("history", 1, 1000),
    ] {
        assert!(
            loads(&format!("[webhooks]\n{key} = {low}\n")),
            "{key} = {low}"
        );
        assert!(
            loads(&format!("[webhooks]\n{key} = {high}\n")),
            "{key} = {high}"
        );
        assert!(!loads(&format!("[webhooks]\n{key} = 0\n")), "{key} = 0");
        assert!(
            !loads(&format!("[webhooks]\n{key} = {}\n", high + 1)),
            "{key} = {}",
            high + 1
        );
    }
}

#[test]
fn unknown_keys_and_wrong_types_fail() {
    for config in [
        "[webhooks]\nport = 7420\n",
        "[webhooks]\nenabled = \"yes\"\n",
        "[webhooks]\nmax_per_hour = \"unlimited\"\n",
        "[webhooks]\nbacklog = -1\n",
    ] {
        assert!(!loads(config), "{config}");
    }
}
