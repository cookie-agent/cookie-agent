use std::fs;

use cookie_agent_config::{CronConfig, load_from_user_root};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn cron_table(enabled: bool, failures: u32, history: usize) -> String {
    format!(
        "[cron]\nenabled = {enabled}\nmax_consecutive_failures = {failures}\nhistory = {history}\n"
    )
}

fn loads(text: &str) -> bool {
    load_from_user_root(Some(root(text).path())).is_ok()
}

#[test]
fn the_cron_table_is_read() {
    let authored = root(&cron_table(false, 5, 40));
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    assert_eq!(
        loaded.runtime.cron,
        CronConfig {
            enabled: false,
            max_consecutive_failures: 5,
            history: 40,
        }
    );
}

#[test]
fn an_authored_table_replaces_the_lower_one_field_by_field() {
    let authored = root("[cron]\nhistory = 7\n");
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    assert_eq!(loaded.runtime.cron.history, 7);
    assert_eq!(
        loaded.runtime.cron.max_consecutive_failures,
        CronConfig::default().max_consecutive_failures
    );
}

#[test]
fn failure_limits_and_history_have_bounds() {
    assert!(!loads(&cron_table(true, 0, 20)));
    assert!(loads(&cron_table(true, 1, 20)));
    assert!(!loads(&cron_table(true, 3, 0)));
    assert!(loads(&cron_table(true, 3, 1)));
    assert!(loads(&cron_table(true, 3, CronConfig::MAX_HISTORY)));
    assert!(!loads(&cron_table(true, 3, CronConfig::MAX_HISTORY + 1)));
}

#[test]
fn unknown_cron_fields_fail() {
    assert!(!loads("[cron]\nenabled = true\ncatch_up = true\n"));
    assert!(!loads("[cron]\nenabled = \"yes\"\n"));
    assert!(!loads("[runtime.cron]\nenabled = true\n"));
}
