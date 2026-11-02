//! Template documents, `_<name>.md`: inherited, never agents
//! (`docs/site/specs/acp-onboarding.md` section 6.1), and the documents left
//! out for a missing `inherit` target, kept so clients can list them.

use std::fs;

use cookie_agent_config::{
    AgentMode, ConfigDocument, ConfigError, ConfigPath, PermissionKey, load_from_user_root,
};
use cookie_agent_identity::AgentId;

fn root() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), "").expect("config");
    directory
}

fn write(root: &tempfile::TempDir, path: &str, text: &str) {
    let path = root.path().join(path);
    fs::create_dir_all(path.parent().unwrap()).expect("folder");
    fs::write(path, text).expect("document");
}

fn id(text: &str) -> AgentId {
    AgentId::new(text).unwrap()
}

const OFFICIAL_ACP: &str = "---\ndescription: An ACP agent driven by cookie\nmode: all\nacp: {}\npermissions:\n  read: allow\n  bash:\n    \"git status*\": allow\n    \"*\": ask\n---\nYou run inside cookie.\n";

#[test]
fn template_paths_are_their_own_kind_of_document() {
    let path = ConfigPath::new("agents/_acp.md").expect("a template");
    assert_eq!(
        path.document(),
        ConfigDocument::Template {
            preset: None,
            name: "acp"
        }
    );
    let path = ConfigPath::new("agents/review/_acp.md").expect("a preset's template");
    assert_eq!(
        path.document(),
        ConfigDocument::Template {
            preset: Some("review"),
            name: "acp"
        }
    );
    for refused in ["agents/_.md", "agents/__acp.md", "agents/_Acp.md"] {
        assert!(
            matches!(ConfigPath::new(refused), Err(ConfigError::AgentFilename(_))),
            "{refused}"
        );
    }
}

#[test]
fn an_official_template_loads_as_a_template_and_never_as_an_agent() {
    let directory = root();
    write(&directory, "official_agents/_acp.md", OFFICIAL_ACP);
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert!(loaded.agents.is_empty(), "{:?}", loaded.agents.keys());
    let template = &loaded.templates[&id("acp")];
    assert!(template.official);
    assert_eq!(template.body, "You run inside cookie.\n");
    assert!(
        loaded
            .agent_registry()
            .template("acp")
            .is_some_and(|template| template.frontmatter.acp.is_some())
    );
    // `[agents] official = false` leaves it unused.
    write(&directory, "config.toml", "[agents]\nofficial = false\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert!(loaded.templates.is_empty());
}

#[test]
fn the_users_template_replaces_or_inherits_the_official_one() {
    let directory = root();
    write(&directory, "official_agents/_acp.md", OFFICIAL_ACP);
    write(
        &directory,
        "agents/_acp.md",
        "---\ninherit: ../official_agents/_acp.md\npermissions:\n  bash: allow\n---\n{{default_prompt}}\n\nPrefer small commits.\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let template = &loaded.templates[&id("acp")];
    assert!(!template.official);
    assert_eq!(
        template.body,
        "You run inside cookie.\n\nPrefer small commits.\n"
    );
    assert!(
        template
            .frontmatter
            .permissions
            .contains_key(&"read".parse::<PermissionKey>().unwrap()),
        "kept from the official template"
    );
    assert!(loaded.agents.is_empty());

    // Without `inherit`, `{{default_prompt}}` is the official template's.
    write(
        &directory,
        "agents/_acp.md",
        "---\ndescription: Mine\nmode: all\nacp: {}\n---\n{{default_prompt}}\nAnd mine.\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let template = &loaded.templates[&id("acp")];
    assert_eq!(template.frontmatter.description, "Mine");
    assert_eq!(template.body, "You run inside cookie.\nAnd mine.\n");
}

#[test]
fn an_agent_may_inherit_a_template_and_a_preset_has_its_own() {
    let directory = root();
    write(&directory, "official_agents/_acp.md", OFFICIAL_ACP);
    write(
        &directory,
        "agents/claude.md",
        "---\ninherit: ../official_agents/_acp.md\nacp: { mode: plan }\n---\n",
    );
    write(
        &directory,
        "agents/review/_acp.md",
        "---\ninherit: ../../official_agents/_acp.md\ndescription: Reviewing\n---\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let claude = &loaded.agents[&id("claude")];
    assert_eq!(claude.frontmatter.mode, AgentMode::All);
    assert_eq!(claude.body, "You run inside cookie.\n");
    assert_eq!(
        claude
            .frontmatter
            .acp
            .as_ref()
            .and_then(|acp| acp.mode.as_deref()),
        Some("plan")
    );
    let presets = loaded.agent_preset_registries();
    assert_eq!(
        presets["review"]
            .template("acp")
            .map(|template| template.frontmatter.description.as_str()),
        Some("Reviewing")
    );
    assert_eq!(
        loaded
            .agent_registry()
            .template("acp")
            .map(|template| template.frontmatter.description.as_str()),
        Some("An ACP agent driven by cookie")
    );
}

#[test]
fn a_template_of_the_users_must_parse() {
    let directory = root();
    write(&directory, "agents/_acp.md", "---\nmode: all\n---\n");
    assert!(load_from_user_root(Some(directory.path())).is_err());
}

#[test]
fn a_document_whose_inherit_target_is_missing_is_kept_as_pending() {
    let directory = root();
    write(
        &directory,
        "agents/primary.md",
        "---\ninherit: ../official_agents/primary.md\ndescription: My assistant\nmode: primary\n---\n",
    );
    write(
        &directory,
        "agents/review/coder.md",
        "---\ninherit: ../../official_agents/coder.md\n---\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("never a load error");
    assert!(loaded.agents.is_empty());
    let shared = loaded.agent_registry();
    let pending = shared.pending();
    assert_eq!(pending.len(), 1, "a preset's pending agent is the preset's");
    assert_eq!(pending[0].id, id("primary"));
    assert_eq!(pending[0].target, "official_agents/primary.md");
    assert_eq!(pending[0].description.as_deref(), Some("My assistant"));
    assert_eq!(pending[0].mode, Some(AgentMode::Primary));
    let presets = loaded.agent_preset_registries();
    let mut ids = presets["review"]
        .pending()
        .iter()
        .map(|pending| pending.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, ["coder", "primary"]);
}
