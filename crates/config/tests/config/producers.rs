use std::fs;

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn producer_body_limit_is_read_from_the_producers_table() {
    let authored = root("[producers]\nmax_body_bytes = 1024\n");
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    assert_eq!(loaded.runtime.producers.max_body_bytes, 1024);
}

#[test]
fn producer_body_limit_must_be_within_the_frame() {
    for (value, valid) in [(0, false), (1, true), (4_194_304, true), (4_194_305, false)] {
        let authored = root(&format!("[producers]\nmax_body_bytes = {value}\n"));
        assert_eq!(
            load_from_user_root(Some(authored.path())).is_ok(),
            valid,
            "max_body_bytes = {value}"
        );
    }
}

#[test]
fn producers_table_rejects_unknown_keys() {
    let authored = root("[producers]\nmax_bodies = 1\n");
    assert!(load_from_user_root(Some(authored.path())).is_err());
}
