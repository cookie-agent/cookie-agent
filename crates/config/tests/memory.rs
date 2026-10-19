use std::fs;

use cookie_agent_config::{MemoryConfig, load_from_user_root};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn memory_table(max_file: usize, user: usize, index: usize) -> String {
    format!(
        "[memory]\nmax_file_bytes = {max_file}\nsnapshot_user_bytes = {user}\nsnapshot_index_bytes = {index}\n"
    )
}

#[test]
fn memory_limits_are_read_from_the_memory_table() {
    let authored = root(&memory_table(2048, 8192, 1024));
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    assert_eq!(
        loaded.runtime.memory,
        MemoryConfig {
            max_file_bytes: 2048,
            snapshot_user_bytes: 8192,
            snapshot_index_bytes: 1024,
        }
    );
}

#[test]
fn memory_limits_must_be_positive() {
    for (max_file, user, index) in [(0, 1, 1), (1, 0, 1), (1, 1, 0)] {
        let authored = root(&memory_table(max_file, user, index));
        assert!(
            load_from_user_root(Some(authored.path())).is_err(),
            "{max_file}/{user}/{index}"
        );
    }
    let authored = root(&memory_table(1, 1, 1));
    assert!(load_from_user_root(Some(authored.path())).is_ok());
}

#[test]
fn memory_limits_with_the_rendering_overhead_stay_under_the_event_bound() {
    assert_eq!(
        MemoryConfig::MAX_LIMITS_SUM + MemoryConfig::RENDER_OVERHEAD_BYTES,
        65_536
    );
    let largest = MemoryConfig::MAX_LIMITS_SUM;
    for ((max_file, user, index), valid) in [
        ((largest - 2, 1, 1), true),
        ((largest - 1, 1, 1), false),
        ((1, largest - 2, 1), true),
        ((1, largest - 1, 1), false),
        ((1, 1, largest - 2), true),
        ((1, 1, largest - 1), false),
        (
            (i64::MAX as usize, i64::MAX as usize, i64::MAX as usize),
            false,
        ),
    ] {
        let authored = root(&memory_table(max_file, user, index));
        assert_eq!(
            load_from_user_root(Some(authored.path())).is_ok(),
            valid,
            "{max_file}/{user}/{index}"
        );
    }
}

#[test]
fn memory_table_rejects_unknown_keys_and_wrong_types() {
    for text in [
        "[memory]\nmax_bytes = 1\n",
        "[memory]\nmax_file_bytes = \"4096\"\n",
        "[memory]\nmax_file_bytes = -1\n",
        "memory = 1\n",
    ] {
        let authored = root(text);
        assert!(
            load_from_user_root(Some(authored.path())).is_err(),
            "{text}"
        );
    }
}
