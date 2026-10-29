use std::fs;

use cookie_agent_config::{DEFERRED_BUILTIN_GROUPS, load_from_user_root};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn loads(text: &str) -> bool {
    load_from_user_root(Some(root(text).path())).is_ok()
}

#[test]
fn always_load_names_the_groups_sent_up_front() {
    let authored = root("[deferred_tools]\nalways_load = [\"memory\", \"cron\"]\n");
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    let config = &loaded.runtime.deferred_tools;
    assert!(config.always_loads("memory"));
    assert!(config.always_loads("cron"));
    assert!(!config.always_loads("goal"));
}

#[test]
fn always_load_takes_every_deferred_group_and_nothing_else() {
    let every = DEFERRED_BUILTIN_GROUPS
        .iter()
        .map(|group| format!("\"{group}\""))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(loads(&format!(
        "[deferred_tools]\nalways_load = [{every}]\n"
    )));
    assert!(loads("[deferred_tools]\nalways_load = []\n"));
    // A group the policy never defers, and one that does not exist.
    assert!(!loads("[deferred_tools]\nalways_load = [\"coding\"]\n"));
    assert!(!loads("[deferred_tools]\nalways_load = [\"Memory\"]\n"));
    assert!(!loads(
        "[deferred_tools]\nalways_load = [\"memory\", \"memory\"]\n"
    ));
}

#[test]
fn unknown_deferred_tools_fields_fail() {
    assert!(!loads("[deferred_tools]\nalways_load = \"memory\"\n"));
    assert!(!loads("[deferred_tools]\nnever_load = [\"memory\"]\n"));
    assert!(!loads("[runtime.deferred_tools]\nalways_load = []\n"));
}
