use std::fs;

use cookie_agent_config::{ConfigError, load_from_roots};
use tempfile::TempDir;

fn load(text: &str) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("config.toml"), text).unwrap();
    load_from_roots(None, Some(directory.path()))
}

#[test]
fn agent_md_defaults_enabled_and_is_strictly_bounded() {
    let defaults = load("").unwrap().runtime.agent_md;
    assert!(defaults.enabled);
    assert_eq!(defaults.max_bytes, 32 * 1024);

    let configured = load("[agent_md]\nenabled = false\nmax_bytes = 17\n").unwrap();
    assert!(!configured.runtime.agent_md.enabled);
    assert_eq!(configured.runtime.agent_md.max_bytes, 17);

    assert!(matches!(
        load("[agent_md]\nunknown = true\n"),
        Err(ConfigError::Toml(_))
    ));
    assert!(matches!(
        load("[agent_md]\nmax_bytes = 0\n"),
        Err(ConfigError::InvalidRuntime)
    ));
    assert!(matches!(
        load("[agent_md]\nmax_bytes = 2097153\n"),
        Err(ConfigError::InvalidRuntime)
    ));
}
