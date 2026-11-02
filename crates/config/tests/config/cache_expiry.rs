//! Compaction before the prompt cache expires
//! (`docs/site/specs/cache-aware-idle.md` section 5):
//! `[context_compaction.cache_expiry]` and the `cache_ttl` of models and
//! ACP agents.

use std::{fs, time::Duration};

use cookie_agent_config::{
    CacheExpiryConfig, CacheExpiryMode, ConfigError, LoadedConfiguration, load_from_user_root,
};
use cookie_agent_identity::ModelKey;

fn load(text: &str) -> Result<LoadedConfiguration, ConfigError> {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), text).expect("config");
    load_from_user_root(Some(directory.path()))
}

#[test]
fn the_table_reads_every_mode_and_its_floors() {
    let loaded = load(
        "[context_compaction.cache_expiry]\nmode = \"always\"\nslack = \"90s\"\nfallback_ttl = \"10m\"\nmin_tokens = -1\nmin_cost_usd = 0.25\n",
    )
    .unwrap();
    let expiry = &loaded.runtime.context_compaction.cache_expiry;
    assert_eq!(expiry.mode, CacheExpiryMode::Always);
    assert_eq!(expiry.slack, Duration::from_secs(90));
    assert_eq!(expiry.fallback_ttl, Duration::from_secs(600));
    assert_eq!(expiry.min_tokens, -1);
    assert!((expiry.min_cost_usd - 0.25).abs() < f64::EPSILON);
    for mode in ["off", "scheduled", "always"] {
        let parsed: CacheExpiryConfig = toml::from_str(&format!("mode = \"{mode}\"")).unwrap();
        assert_eq!(format!("{:?}", parsed.mode).to_lowercase(), mode);
    }
}

#[test]
fn the_table_refuses_unknown_fields_and_bad_values() {
    for setting in [
        "mode = \"sometimes\"",
        "slack = \"5\"",
        "slack = \"0s\"",
        "fallback_ttl = \"0s\"",
        "fallback_ttl = \"-1m\"",
        "min_tokens = \"40000\"",
        "min_cost_usd = \"0.5\"",
        "modes = \"off\"",
    ] {
        let text = format!("[context_compaction.cache_expiry]\n{setting}\n");
        assert!(load(&text).is_err(), "accepted {setting}");
    }
}

#[test]
fn the_slack_is_capped_at_a_fifth_of_the_ttl() {
    let expiry: CacheExpiryConfig = toml::from_str("slack = \"5m\"").unwrap();
    assert_eq!(
        expiry.effective_slack(Duration::from_secs(3_600)),
        Duration::from_secs(300)
    );
    assert_eq!(
        expiry.effective_slack(Duration::from_secs(600)),
        Duration::from_secs(120)
    );
    assert_eq!(
        expiry.effective_slack(Duration::from_secs(300)),
        Duration::from_secs(60)
    );
}

const MODEL: &str = r#"
[providers.test]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }
setup = {}

[providers.test.models.model]
display_name = "Model"
CACHE_TTL

[providers.test.models.model.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 4096
output_tokens = 1024
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = false
native_replay = "unsupported"
media = {}
"#;

#[test]
fn a_model_declares_its_cache_ttl_and_zero_never_expires() {
    let key: ModelKey = "test/model".parse().unwrap();
    for (authored, expected) in [("2h", 7_200), ("0s", 0)] {
        let loaded =
            load(&MODEL.replace("CACHE_TTL", &format!("cache_ttl = \"{authored}\""))).unwrap();
        assert_eq!(
            loaded.runtime.model_cache_ttls.get(&key),
            Some(&Duration::from_secs(expected))
        );
    }
    let without = load(&MODEL.replace("CACHE_TTL", "")).unwrap();
    assert!(without.runtime.model_cache_ttls.is_empty());
    assert!(load(&MODEL.replace("CACHE_TTL", "cache_ttl = \"an hour\"")).is_err());
}

#[test]
fn an_acp_agent_declares_its_cache_ttl() {
    let loaded =
        load("[acp.agents.claude]\ncommand = [\"claude-agent-acp\"]\ncache_ttl = \"1h\"\n")
            .unwrap();
    let agent = &loaded.runtime.acp.agents[&cookie_agent_identity::AgentId::new("claude").unwrap()];
    assert_eq!(agent.cache_ttl_duration(), Some(Duration::from_secs(3_600)));
    assert!(
        load("[acp.agents.claude]\ncommand = [\"claude-agent-acp\"]\ncache_ttl = \"soon\"\n")
            .is_err()
    );
}
