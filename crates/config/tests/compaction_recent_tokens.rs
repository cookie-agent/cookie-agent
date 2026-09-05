use std::fs;

use cookie_agent_config::{
    ConfigError, ContextCompactionConfig, ContextCompactionTrigger, load_from_roots,
};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn recent_tokens_defaults_match_rust_serde_and_loader() {
    assert_eq!(
        ContextCompactionConfig::default().keep_recent_tokens,
        16_384
    );
    let parsed: ContextCompactionConfig = toml::from_str("").unwrap();
    assert_eq!(parsed.keep_recent_tokens, 16_384);
    assert_eq!(
        load_from_roots(None, None)
            .unwrap()
            .runtime
            .context_compaction
            .keep_recent_tokens,
        16_384
    );
    for config in [
        "",
        "[context_compaction]\n",
        "[context_compaction]\nauto = false\n",
    ] {
        let directory = root(config);
        let loaded = load_from_roots(Some(directory.path()), None).unwrap();
        assert_eq!(loaded.runtime.context_compaction.keep_recent_tokens, 16_384);
    }
}

#[test]
fn recent_tokens_accept_zero_and_large_budgets_without_clamping() {
    for tokens in [0, 1, 16_384, i64::MAX as u64] {
        let setting = format!("keep_recent_tokens = {tokens}\n");
        let parsed: ContextCompactionConfig = toml::from_str(&setting).unwrap();
        assert_eq!(parsed.keep_recent_tokens, tokens);
        let directory = root(&format!("[context_compaction]\n{setting}"));
        let loaded = load_from_roots(Some(directory.path()), None).unwrap();
        assert_eq!(loaded.runtime.context_compaction.keep_recent_tokens, tokens);
    }
    let parsed: ContextCompactionConfig =
        serde_json::from_value(serde_json::json!({ "keep_recent_tokens": u64::MAX })).unwrap();
    assert_eq!(parsed.keep_recent_tokens, u64::MAX);
}

#[test]
fn recent_tokens_reject_invalid_values_and_unknown_fields() {
    for setting in [
        "keep_recent_tokens = -1",
        "keep_recent_tokens = 1.5",
        "keep_recent_tokens = \"16384\"",
        "keep_recent_tokens = true",
        "keep_recent_tokens = []",
        "keep_recent_tokens = {}",
        "keep_recent_tokens = 18446744073709551616",
        "keep_recent_tokens = 0\nkeep_recent_token = 1",
        "keep_recent_tokens = 0\nschema = 1",
        "keep_recent_tokens = 0\nschema_version = 1",
        "keep_recent_tokens = 0\nkeep_recent_tokens = 1",
    ] {
        assert!(
            toml::from_str::<ContextCompactionConfig>(setting).is_err(),
            "accepted {setting}"
        );
        let directory = root(&format!("[context_compaction]\n{setting}\n"));
        assert!(
            matches!(
                load_from_roots(Some(directory.path()), None),
                Err(ConfigError::Toml(_))
            ),
            "accepted {setting}"
        );
    }
}

#[test]
fn recent_tokens_are_independent_of_trigger_and_summary_limits() {
    let directory =
        root("[context_compaction]\nbuffer_tokens = 33000\nkeep_recent_tokens = 8192\n");
    let loaded = load_from_roots(Some(directory.path()), None).unwrap();
    assert_eq!(loaded.runtime.context_compaction.keep_recent_tokens, 8192);
    assert_eq!(
        loaded.runtime.context_compaction.trigger,
        ContextCompactionTrigger::BufferTokens {
            buffer_tokens: 33_000
        }
    );

    for invalid in ["buffer_tokens = 0", "max_summary_bytes = 0"] {
        let directory = root(&format!(
            "[context_compaction]\nkeep_recent_tokens = 0\n{invalid}\n"
        ));
        assert!(matches!(
            load_from_roots(Some(directory.path()), None),
            Err(ConfigError::InvalidRuntime)
        ));
    }
}

#[test]
fn recent_tokens_follow_section_replacement_semantics() {
    let user = root("[context_compaction]\nkeep_recent_tokens = 8192\n");
    let workspace = root("[context_compaction]\nkeep_recent_tokens = 0\n");
    let loaded = load_from_roots(Some(user.path()), Some(workspace.path())).unwrap();
    assert_eq!(loaded.runtime.context_compaction.keep_recent_tokens, 0);

    let workspace = root("[context_compaction]\nauto = false\n");
    let loaded = load_from_roots(Some(user.path()), Some(workspace.path())).unwrap();
    assert_eq!(loaded.runtime.context_compaction.keep_recent_tokens, 16_384);
    assert!(!loaded.runtime.context_compaction.auto_compaction);

    let invalid_user = root("[context_compaction]\nkeep_recent_tokens = -1\n");
    assert!(matches!(
        load_from_roots(Some(invalid_user.path()), Some(workspace.path())),
        Err(ConfigError::Toml(_))
    ));
}
