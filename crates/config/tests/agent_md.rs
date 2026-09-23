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

fn load_agent(mode: &str, agent_md: Option<&str>) -> Result<Option<bool>, ConfigError> {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("config.toml"), "").unwrap();
    let agents = directory.path().join("agents");
    fs::create_dir_all(&agents).unwrap();
    let knob = agent_md.map_or_else(String::new, |value| format!("agent_md: {value}\n"));
    fs::write(
        agents.join("worker.md"),
        format!(
            "---\ndescription: Worker\nmode: {mode}\nenabled: true\nmodels: [{{ model: \"custom.test/model\" }}]\n{knob}permissions: {{}}\n---\nWork.\n"
        ),
    )
    .unwrap();
    let loaded = load_from_roots(None, Some(directory.path()))?;
    Ok(loaded
        .agents
        .get(&cookie_agent_identity::AgentId::new("worker").unwrap())
        .unwrap()
        .frontmatter
        .agent_md)
}

#[test]
fn agent_document_agent_md_defaults_to_global_and_accepts_booleans() {
    assert_eq!(load_agent("primary", None).unwrap(), None);
    assert_eq!(load_agent("primary", Some("false")).unwrap(), Some(false));
    assert_eq!(load_agent("all", Some("true")).unwrap(), Some(true));
}

#[test]
fn agent_document_agent_md_is_strict() {
    assert!(matches!(
        load_agent("primary", Some("\"no\"")),
        Err(ConfigError::AgentDocument { .. })
    ));
    assert!(matches!(
        load_agent("primary", Some("{ enabled: false }")),
        Err(ConfigError::AgentDocument { .. })
    ));
    // Only root runs load AGENTS.md; agents that cannot be roots reject it.
    for mode in ["subagent", "internal"] {
        assert!(matches!(
            load_agent(mode, Some("false")),
            Err(ConfigError::AgentField {
                field: "agent_md",
                ..
            })
        ));
    }
}
