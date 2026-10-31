use std::fs;

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn messaging_is_top_level_not_nested_under_runtime() {
    // The canonical spelling parses.
    let top_level = root("[messaging]\nenabled = false\n");
    assert!(load_from_user_root(Some(top_level.path())).is_ok());

    // The `[runtime.messaging]` spelling used in older docs is an unknown
    // top-level field `runtime` and must hard-fail strict validation.
    let nested = root("[runtime.messaging]\nenabled = false\n");
    assert!(load_from_user_root(Some(nested.path())).is_err());
}

#[test]
fn messaging_is_strict_about_unknown_keys_and_wrong_types() {
    let unknown = root("[messaging]\nunknown = 1\n");
    assert!(load_from_user_root(Some(unknown.path())).is_err());

    let wrong_type = root("[messaging]\nmax_body_bytes = \"32768\"\n");
    assert!(load_from_user_root(Some(wrong_type.path())).is_err());

    let negative_size = root("[messaging]\nmax_pending_per_session = -1\n");
    assert!(load_from_user_root(Some(negative_size.path())).is_err());

    let float = root("[messaging]\nmax_hops = 1.5\n");
    assert!(load_from_user_root(Some(float.path())).is_err());
}

#[test]
fn messaging_boundaries_parse_and_layer_replaces_whole_sections() {
    let unlimited = root(
        "[messaging]\nmax_hops = -7\nmax_body_bytes = 0\nmax_pending_per_session = 0\nmax_inflight_per_pair = 0\n",
    );
    let loaded = load_from_user_root(Some(unlimited.path())).expect("boundary values");
    assert_eq!(loaded.runtime.messaging.max_hops, -7);
    // Zero is valid at parse time; the send path rejects every body against a
    // zero byte cap.
    assert_eq!(loaded.runtime.messaging.max_body_bytes, 0);
    assert_eq!(loaded.runtime.messaging.max_pending_per_session, 0);
    assert_eq!(loaded.runtime.messaging.max_inflight_per_pair, 0);

    let disabled = root("[messaging]\nenabled = false\nmax_hops = 2\n");
    let loaded = load_from_user_root(Some(disabled.path())).expect("disabled");
    assert!(!loaded.runtime.messaging.enabled);
    assert_eq!(loaded.runtime.messaging.max_hops, 2);
}
