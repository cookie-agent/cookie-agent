use std::fs;

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn error(config: &str) -> String {
    load_from_user_root(Some(root(config).path()))
        .err()
        .unwrap_or_else(|| panic!("{config} should be rejected"))
        .to_string()
}

#[test]
fn preview_tables_are_strict_about_unknown_keys_and_types() {
    for table in ["tool_output", "subagent_output"] {
        error(&format!("[{table}]\nunknown = 1\n"));
        error(&format!("[{table}]\nhead_lines = \"10\"\n"));
        error(&format!("[{table}]\ntail_lines = -1\n"));
    }
}

#[test]
fn omitted_kept_lines_are_capped_by_the_room_max_lines_leaves() {
    // Both omitted: each end gets at most half (the start rounded up). One
    // omitted: it gets at most what the other leaves.
    for (table, expected) in [
        ("max_lines = 7\n", (4, 3)),
        ("max_lines = 1\n", (1, 0)),
        ("max_lines = 10\nhead_lines = 8\n", (8, 2)),
        ("max_lines = 10\ntail_lines = 7\n", (3, 7)),
        ("max_lines = 4\nhead_lines = 0\n", (0, 4)),
        ("max_lines = 10\nhead_lines = 6\ntail_lines = 4\n", (6, 4)),
        ("max_lines = 10\nhead_lines = 10\ntail_lines = 0\n", (10, 0)),
    ] {
        let directory = root(&format!("[tool_output]\n{table}[subagent_output]\n{table}"));
        let loaded = load_from_user_root(Some(directory.path()))
            .unwrap_or_else(|error| panic!("{table}: {error}"));
        assert_eq!(loaded.runtime.tool_output.kept_lines(), expected, "{table}");
        assert_eq!(
            loaded.runtime.subagent_output.kept_lines(),
            expected,
            "{table}"
        );
    }
}

#[test]
fn kept_lines_must_fit_the_limit_and_keep_something() {
    for table in ["tool_output", "subagent_output"] {
        for overflow in [
            "max_lines = 10\nhead_lines = 6\ntail_lines = 5\n",
            "max_lines = 10\nhead_lines = 11\n",
            "max_lines = 10\ntail_lines = 11\n",
        ] {
            assert!(
                error(&format!("[{table}]\n{overflow}")).contains("must not exceed max_lines"),
                "{table}: {overflow}"
            );
        }
        assert!(
            error(&format!("[{table}]\nhead_lines = 0\ntail_lines = 0\n"))
                .contains("at least one line")
        );
        assert!(error(&format!("[{table}]\nmax_lines = 0\n")).contains("max_lines"));
        assert!(error(&format!("[{table}]\nmax_bytes = 0\n")).contains("max_bytes"));
    }
}

#[test]
fn subagent_previews_fit_the_event_bound() {
    let directory = root("[subagent_output]\nmax_bytes = 61440\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("largest preview");
    assert_eq!(loaded.runtime.subagent_output.max_bytes, 61_440);
    assert!(error("[subagent_output]\nmax_bytes = 61441\n").contains("1..=61440"));
    // Tool output has no such ceiling of its own.
    let directory = root("[tool_output]\nmax_bytes = 1048576\n");
    load_from_user_root(Some(directory.path())).expect("large tool previews");
}
