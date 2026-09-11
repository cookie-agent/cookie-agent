use std::fs;

use cookie_agent_config::load_from_roots;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn messaging_defaults_apply_when_the_section_is_absent() {
    let defaults = root("");
    let loaded = load_from_roots(Some(defaults.path()), None).expect("defaults");
    assert!(loaded.runtime.messaging.enabled);
    assert_eq!(loaded.runtime.messaging.max_hops, 0);
    assert_eq!(loaded.runtime.messaging.max_body_bytes, 32_768);
    assert_eq!(loaded.runtime.messaging.max_pending_per_session, 32);
    assert_eq!(loaded.runtime.messaging.max_inflight_per_pair, 4);

    let empty = root("[messaging]\n");
    let loaded = load_from_roots(Some(empty.path()), None).expect("empty section");
    assert!(loaded.runtime.messaging.enabled);
    assert_eq!(loaded.runtime.messaging.max_body_bytes, 32_768);
}

#[test]
fn messaging_is_strict_about_unknown_keys_and_wrong_types() {
    let unknown = root("[messaging]\nunknown = 1\n");
    assert!(load_from_roots(Some(unknown.path()), None).is_err());

    let wrong_type = root("[messaging]\nmax_body_bytes = \"32768\"\n");
    assert!(load_from_roots(Some(wrong_type.path()), None).is_err());

    let negative_size = root("[messaging]\nmax_pending_per_session = -1\n");
    assert!(load_from_roots(Some(negative_size.path()), None).is_err());

    let float = root("[messaging]\nmax_hops = 1.5\n");
    assert!(load_from_roots(Some(float.path()), None).is_err());
}

#[test]
fn messaging_boundaries_parse_and_layer_replaces_whole_sections() {
    let unlimited = root(
        "[messaging]\nmax_hops = -7\nmax_body_bytes = 0\nmax_pending_per_session = 0\nmax_inflight_per_pair = 0\n",
    );
    let loaded = load_from_roots(Some(unlimited.path()), None).expect("boundary values");
    assert_eq!(loaded.runtime.messaging.max_hops, -7);
    // Zero is valid at parse time; the send path rejects every body against a
    // zero byte cap.
    assert_eq!(loaded.runtime.messaging.max_body_bytes, 0);
    assert_eq!(loaded.runtime.messaging.max_pending_per_session, 0);
    assert_eq!(loaded.runtime.messaging.max_inflight_per_pair, 0);

    let disabled = root("[messaging]\nenabled = false\nmax_hops = 2\n");
    let loaded = load_from_roots(Some(disabled.path()), None).expect("disabled");
    assert!(!loaded.runtime.messaging.enabled);
    assert_eq!(loaded.runtime.messaging.max_hops, 2);

    let user = root("[messaging]\nenabled = false\nmax_body_bytes = 4096\n");
    let workspace = root("[messaging]\nmax_hops = 2\n");
    let loaded =
        load_from_roots(Some(user.path()), Some(workspace.path())).expect("layered config");
    // Authored layers replace wholesale; omitted fields fall back to defaults.
    assert!(loaded.runtime.messaging.enabled);
    assert_eq!(loaded.runtime.messaging.max_hops, 2);
    assert_eq!(loaded.runtime.messaging.max_body_bytes, 32_768);
}
