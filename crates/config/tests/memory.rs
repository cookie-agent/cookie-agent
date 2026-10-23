use std::fs;

use cookie_agent_config::{MemoryConfig, load_from_user_root};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

/// A `[memory]` table with every limit given: the five document limits,
/// then the two snapshot limits.
fn memory_table(documents: [usize; 5], user: usize, index: usize) -> String {
    let [user_max, agent_max, machine_max, project_max, overview_max] = documents;
    format!(
        "[memory]\nuser_max_bytes = {user_max}\nagent_max_bytes = {agent_max}\nmachine_max_bytes = {machine_max}\nproject_max_bytes = {project_max}\noverview_max_bytes = {overview_max}\nsnapshot_user_bytes = {user}\nsnapshot_index_bytes = {index}\n"
    )
}

fn loads(text: &str) -> bool {
    load_from_user_root(Some(root(text).path())).is_ok()
}

#[test]
fn memory_limits_are_read_from_the_memory_table() {
    let authored = root(&memory_table([1000, 1100, 1200, 1300, 1400], 8192, 1024));
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    assert_eq!(
        loaded.runtime.memory,
        MemoryConfig {
            user_max_bytes: 1000,
            agent_max_bytes: 1100,
            machine_max_bytes: 1200,
            project_max_bytes: 1300,
            overview_max_bytes: 1400,
            snapshot_user_bytes: 8192,
            snapshot_index_bytes: 1024,
        }
    );
}

#[test]
fn memory_limits_must_be_positive() {
    for zero in 0..7 {
        let mut limits = [1; 7];
        limits[zero] = 0;
        let documents = [limits[0], limits[1], limits[2], limits[3], limits[4]];
        assert!(
            !loads(&memory_table(documents, limits[5], limits[6])),
            "limit {zero}"
        );
    }
    assert!(loads(&memory_table([1; 5], 1, 1)));
}

#[test]
fn memory_limits_with_the_rendering_overhead_stay_under_the_event_bound() {
    assert_eq!(
        MemoryConfig::MAX_LIMITS_SUM + MemoryConfig::RENDER_OVERHEAD_BYTES,
        65_536
    );
    let largest = MemoryConfig::MAX_LIMITS_SUM;
    for ((agent, user, index), valid) in [
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
        assert_eq!(
            loads(&memory_table([1, agent, 1, 1, 1], user, index)),
            valid,
            "{agent}/{user}/{index}"
        );
    }
    // The other document limits are not summed, but none may pass the
    // largest a document is read at.
    for document in [0, 2, 3, 4] {
        let mut documents = [1; 5];
        documents[document] = largest;
        assert!(
            loads(&memory_table(documents, largest - 2, 1)),
            "{document}"
        );
        documents[document] = largest + 1;
        assert!(!loads(&memory_table(documents, 1, 1)), "{document}");
    }
}

#[test]
fn memory_table_rejects_unknown_keys_and_wrong_types() {
    for text in [
        "[memory]\nmax_file_bytes = 4096\n",
        "[memory]\nmax_bytes = 1\n",
        "[memory]\nuser_max_bytes = \"2048\"\n",
        "[memory]\nagent_max_bytes = -1\n",
        "memory = 1\n",
    ] {
        assert!(!loads(text), "{text}");
    }
}
