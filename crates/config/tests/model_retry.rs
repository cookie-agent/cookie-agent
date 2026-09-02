use std::fs;

use cookie_agent_config::load_from_roots;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn model_retry_defaults_and_signed_counts_are_accepted() {
    let defaults = root("");
    let loaded = load_from_roots(Some(defaults.path()), None).expect("defaults");
    assert_eq!(loaded.runtime.model_retry.standard_retries, 3);
    assert_eq!(loaded.runtime.model_retry.overload_retries, 5);
    assert_eq!(loaded.runtime.model_retry.backoff_ceiling_ms, 60_000);

    let configured = root(
        "[model_retry]\nstandard_retries = -1\noverload_retries = 0\nbackoff_ceiling_ms = 1\n",
    );
    let loaded = load_from_roots(Some(configured.path()), None).expect("signed retry counts");
    assert_eq!(loaded.runtime.model_retry.standard_retries, -1);
    assert_eq!(loaded.runtime.model_retry.overload_retries, 0);
    assert_eq!(loaded.runtime.model_retry.backoff_ceiling_ms, 1);
}

#[test]
fn model_retry_is_strict_and_workspace_replaces_the_section() {
    let unknown = root("[model_retry]\nunknown = 1\n");
    assert!(load_from_roots(Some(unknown.path()), None).is_err());

    let user = root("[model_retry]\nstandard_retries = -1\noverload_retries = -1\n");
    let workspace = root("[model_retry]\noverload_retries = 0\n");
    let loaded =
        load_from_roots(Some(user.path()), Some(workspace.path())).expect("layered config");
    assert_eq!(loaded.runtime.model_retry.standard_retries, 3);
    assert_eq!(loaded.runtime.model_retry.overload_retries, 0);
    assert_eq!(loaded.runtime.model_retry.backoff_ceiling_ms, 60_000);

    let zero_ceiling = root("[model_retry]\nbackoff_ceiling_ms = 0\n");
    assert!(load_from_roots(Some(zero_ceiling.path()), None).is_err());
}
