use std::fs;

use cookie_agent_config::{ConfigError, load_from_roots};
use cookie_agent_models::adapters::AnthropicCacheTtlConfig;
use tempfile::TempDir;

fn load(text: &str) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("config.toml"), text).unwrap();
    load_from_roots(None, Some(directory.path()))
}

#[test]
fn prompt_caching_defaults_enabled_with_ordered_ttls() {
    let loaded = load("").unwrap();
    let caching = loaded.runtime.prompt_caching;
    assert!(caching.enabled);
    assert_eq!(caching.system_ttl, AnthropicCacheTtlConfig::OneHour);
    assert_eq!(caching.tools_ttl, AnthropicCacheTtlConfig::OneHour);
    assert_eq!(caching.rolling_ttl, AnthropicCacheTtlConfig::FiveMinutes);
    assert!(caching.strategy().is_some());
}

#[test]
fn prompt_caching_can_be_disabled_and_is_strict() {
    let disabled = load("[prompt_caching]\nenabled = false\n").unwrap();
    assert!(disabled.runtime.prompt_caching.strategy().is_none());

    let unknown = load("[prompt_caching]\nunknown = true\n").unwrap_err();
    assert!(matches!(unknown, ConfigError::Toml(_)));

    let invalid_order =
        load("[prompt_caching]\ntools_ttl = \"five_minutes\"\nsystem_ttl = \"one_hour\"\n")
            .unwrap_err();
    assert!(matches!(invalid_order, ConfigError::InvalidRuntime));
}
