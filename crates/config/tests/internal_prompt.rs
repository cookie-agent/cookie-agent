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
fn approval_and_title_take_no_placeholder() {
    // Their prompts are the official documents' bodies: nothing compiled in
    // is substituted, so `{{default_prompt}}` would send itself literally.
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

        for placeholder in ["default_prompt", "focus"] {
            let root = root_with_agent(
                &temp,
                &format!("{agent}-{placeholder}"),
                &file,
                &format!("{{{{{placeholder}}}}}\nMore."),
            );
            assert!(
                matches!(
                    load(&root),
                    Err(ConfigError::UnknownPromptPlaceholder { agent: id, placeholder: found })
                        if id.as_str() == agent && found == placeholder
                ),
                "{agent} {placeholder}"
            );
        }
    }
}

#[test]
fn non_internal_agents_keep_placeholder_text_literal() {
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
    assert_eq!(
        document.body,
        "Review {{anything}} and {{default_prompt}}.\n"
    );
}
