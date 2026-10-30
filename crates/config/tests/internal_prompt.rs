use std::{fs, path::Path};

use cookie_agent_config::{ConfigError, load_from_user_root};
use cookie_agent_identity::AgentId;
use tempfile::TempDir;

fn root_with_agent(temp: &TempDir, name: &str, file: &str, body: &str) -> std::path::PathBuf {
    let root = temp.path().join(name);
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(root.join("config.toml"), "").unwrap();
    fs::write(
        root.join("agents").join(file),
        format!(
            "---\ndescription: Compaction\nmode: internal\nenabled: true\nmodels: [{{ model: \"${{parent_model}}\" }}]\npermissions: {{}}\n---\n{body}\n"
        ),
    )
    .unwrap();
    root
}

fn load(root: &Path) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    load_from_user_root(Some(root))
}

#[test]
fn compaction_document_accepts_known_placeholders_and_literal_braces() {
    let temp = TempDir::new().unwrap();
    let body = "Keep test names.\n{{ default_prompt }}\nFocus: {{focus}}\nLiteral {{not a placeholder}} and {{";
    let root = root_with_agent(&temp, "known", "compaction.md", body);
    let loaded = load(&root).unwrap();
    let document = loaded
        .agents
        .get(&AgentId::new("compaction").unwrap())
        .unwrap();
    assert_eq!(document.body, format!("{body}\n"));
}

#[test]
fn compaction_document_rejects_unknown_placeholders() {
    let temp = TempDir::new().unwrap();
    let root = root_with_agent(
        &temp,
        "unknown",
        "compaction.md",
        "{{default_prompt}}\n{{defaults}}",
    );
    let error = load(&root).unwrap_err();
    assert!(
        matches!(
            &error,
            ConfigError::UnknownPromptPlaceholder { agent, placeholder }
                if agent.as_str() == "compaction" && placeholder == "defaults"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "agent `compaction` uses unknown prompt placeholder `{{defaults}}`"
    );
}

#[test]
fn approval_and_title_take_default_prompt_but_not_focus() {
    // `{{default_prompt}}` is the official agent's prompt; with no official
    // agent, as here, it stands for nothing (nothing is compiled in).
    let temp = TempDir::new().unwrap();
    for agent in ["approval", "title"] {
        let file = format!("{agent}.md");
        let root = root_with_agent(
            &temp,
            &format!("{agent}-plain"),
            &file,
            "Review calls.\nPrefer ask for network access.",
        );
        let loaded = load(&root).unwrap();
        let document = loaded.agents.get(&AgentId::new(agent).unwrap()).unwrap();
        assert_eq!(
            document.body,
            "Review calls.\nPrefer ask for network access.\n"
        );

        let root = root_with_agent(
            &temp,
            &format!("{agent}-default_prompt"),
            &file,
            "{{default_prompt}}\nMore.",
        );
        let loaded = load(&root).unwrap();
        assert_eq!(loaded.agents[&AgentId::new(agent).unwrap()].body, "More.\n");
        assert_eq!(loaded.official_agent_diagnostics.len(), 1, "{agent}");

        let root = root_with_agent(&temp, &format!("{agent}-focus"), &file, "{{focus}}\nMore.");
        assert!(
            matches!(
                load(&root),
                Err(ConfigError::UnknownPromptPlaceholder { agent: id, placeholder: found })
                    if id.as_str() == agent && found == "focus"
            ),
            "{agent}"
        );
    }
}

#[test]
fn non_internal_agents_keep_other_placeholder_text_literal() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("subagent");
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(root.join("config.toml"), "").unwrap();
    fs::write(
        root.join("agents").join("reviewer.md"),
        "---\ndescription: Reviewer\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/model\" }]\npermissions: {}\n---\nReview {{anything}} and {{default_prompt}}.\n",
    )
    .unwrap();
    let loaded = load(&root).unwrap();
    let document = loaded
        .agents
        .get(&AgentId::new("reviewer").unwrap())
        .unwrap();
    // `{{default_prompt}}` stands for the official `reviewer`'s prompt, and
    // there is none.
    assert_eq!(document.body, "Review {{anything}} and .\n");
}
