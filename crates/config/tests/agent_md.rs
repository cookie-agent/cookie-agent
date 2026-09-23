use std::fs;

use cookie_agent_config::{ConfigError, load_from_roots};
use tempfile::TempDir;

fn load(text: &str) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("config.toml"), text).unwrap();
    load_from_roots(None, Some(directory.path()))
}

#[test]
fn agent_md_defaults_enabled_and_rejects_unknown_fields() {
    let defaults = load("").unwrap().runtime.agent_md;
    assert!(defaults.enabled);

    let configured = load("[agent_md]\nenabled = false\n").unwrap();
    assert!(!configured.runtime.agent_md.enabled);

    assert!(matches!(
        load("[agent_md]\nunknown = true\n"),
        Err(ConfigError::Toml(_))
    ));
    assert!(matches!(
        load("[agent_md]\nmax_bytes = 17\n"),
        Err(ConfigError::Toml(_))
    ));
}
