use std::fs;

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn model_retry_defaults_and_signed_counts_are_accepted() {
    let defaults = root("");
    let loaded = load_from_user_root(Some(defaults.path())).expect("defaults");
    assert_eq!(loaded.runtime.model_retry.standard_retries, 3);
    assert_eq!(loaded.runtime.model_retry.overload_retries, 5);
    assert_eq!(loaded.runtime.model_retry.backoff_ceiling_ms, 60_000);

    let configured = root(
        "[model_retry]\nstandard_retries = -1\noverload_retries = 0\nbackoff_ceiling_ms = 1\n",
    );
    let loaded = load_from_user_root(Some(configured.path())).expect("signed retry counts");
    assert_eq!(loaded.runtime.model_retry.standard_retries, -1);
    assert_eq!(loaded.runtime.model_retry.overload_retries, 0);
    assert_eq!(loaded.runtime.model_retry.backoff_ceiling_ms, 1);
}

#[test]
fn model_retry_is_strict() {
    let unknown = root("[model_retry]\nunknown = 1\n");
    assert!(load_from_user_root(Some(unknown.path())).is_err());

    let zero_ceiling = root("[model_retry]\nbackoff_ceiling_ms = 0\n");
    let error = load_from_user_root(Some(zero_ceiling.path())).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("model_retry.backoff_ceiling_ms must be positive")
    );
}

#[test]
fn malformed_config_reports_location_without_echoing_source_secrets() {
    let config = root("[model_retry]\nstandard_retries = \"private-secret\n");
    let error = load_from_user_root(Some(config.path()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("config.toml"));
    assert!(error.contains("line 2"));
    assert!(!error.contains("private-secret"));
}
