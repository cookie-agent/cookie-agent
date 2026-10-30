//! `[agents]`, the official agents folder below the user root, and
//! `utility_model` (`docs/site/specs/machines-on-demand.md` sections 2.1
//! and 2.2).

use std::fs;

use cookie_agent_config::{AgentModelRef, ConfigTree, load_from_tree, load_from_user_root};
use cookie_agent_identity::AgentId;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn official(root: &tempfile::TempDir, id: &str, text: &str) {
    let folder = root.path().join("official_agents");
    fs::create_dir_all(&folder).expect("official agents");
    fs::write(folder.join(format!("{id}.md")), text).expect("official agent");
}

fn user(root: &tempfile::TempDir, id: &str, text: &str) {
    let folder = root.path().join("agents");
    fs::create_dir_all(&folder).expect("agents");
    fs::write(folder.join(format!("{id}.md")), text).expect("agent");
}

fn agent(description: &str) -> String {
    format!(
        "---\ndescription: {description}\nmode: primary\nmodels: [\"custom.test/model\"]\n---\nWork.\n"
    )
}

fn id(text: &str) -> AgentId {
    AgentId::new(text).unwrap()
}

#[test]
fn the_agents_table_takes_official_and_default() {
    let loaded = load_from_user_root(Some(
        root("[agents]\nofficial = false\ndefault = \"coder\"\n").path(),
    ))
    .expect("loads");
    assert!(!loaded.runtime.agents.official);
    assert_eq!(loaded.runtime.agents.default_agent(), id("coder"));
    // A default no agent has loads: official agents arrive later.
    assert!(!loaded.agents.contains_key(&id("coder")));
}

#[test]
fn the_agents_table_is_strict() {
    for config in [
        "[agents]\nofficial = \"yes\"\n",
        "[agents]\ndefault = \"Not An ID\"\n",
        "[agents]\ndefault = 3\n",
        "[agents]\nsource = \"https://example.test\"\n",
        "agents = true\n",
    ] {
        assert!(
            load_from_user_root(Some(root(config).path())).is_err(),
            "{config:?} must be refused"
        );
    }
}

#[test]
fn official_agents_load_below_the_users_own() {
    let directory = root("");
    official(&directory, "primary", &agent("Official primary"));
    official(&directory, "coder", &agent("Official coder"));
    fs::write(
        directory.path().join("official_agents/.index.json"),
        r#"[{"id": "coder", "version": "1.2.0", "requires_cookie": ">=0.1.0", "sha256": ""}]"#,
    )
    .unwrap();
    user(&directory, "primary", &agent("Mine"));
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let primary = &loaded.agents[&id("primary")];
    assert!(!primary.official, "the user's document replaces it whole");
    assert_eq!(primary.frontmatter.description, "Mine");
    let coder = &loaded.agents[&id("coder")];
    assert!(coder.official);
    assert_eq!(coder.official_version.as_deref(), Some("1.2.0"));
    assert!(
        !loaded.agents.contains_key(&id("index")),
        "the daemon's record is no agent"
    );
    assert!(loaded.official_agent_diagnostics.is_empty());
}

#[test]
fn an_official_agent_that_does_not_load_is_left_out_with_a_diagnostic() {
    let directory = root("");
    official(&directory, "primary", &agent("Fine"));
    official(
        &directory,
        "broken",
        "---\ndescription: Broken\nmode: primary\nfuture_key: 1\n---\nWork.\n",
    );
    official(
        &directory,
        "title",
        "---\ndescription: Not internal\nmode: primary\nmodels: [\"custom.test/model\"]\n---\nWork.\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("never a load error");
    assert!(loaded.agents.contains_key(&id("primary")));
    assert!(!loaded.agents.contains_key(&id("broken")));
    assert!(!loaded.agents.contains_key(&id("title")));
    assert_eq!(loaded.official_agent_diagnostics.len(), 2);
    assert!(
        loaded
            .official_agent_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.starts_with("official agent `")),
        "{:?}",
        loaded.official_agent_diagnostics
    );
}

#[test]
fn official_agents_off_leaves_the_folder_unused() {
    let directory = root("[agents]\nofficial = false\n");
    official(&directory, "primary", &agent("Official"));
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert!(loaded.agents.is_empty());
}

#[test]
fn official_agents_are_not_documents_of_the_tree() {
    let directory = root("");
    official(&directory, "primary", &agent("Official"));
    let tree = ConfigTree::read(Some(directory.path())).expect("tree");
    assert!(
        tree.documents()
            .all(|(path, _)| path.as_str() == "config.toml"),
        "never written through the tree"
    );
    let without = {
        let mut tree = tree.clone();
        tree.clear_official_agents();
        tree
    };
    assert_eq!(tree.revision(), without.revision());
    assert!(
        load_from_tree(&tree)
            .unwrap()
            .agents
            .contains_key(&id("primary"))
    );
}

#[test]
fn utility_model_is_a_model_value_agents_reach_as_a_variable() {
    let directory = root(
        "utility_model = { model = \"deepseek/deepseek-flash\", variant = \"off\" }\nsmall_model = \"deepseek/deepseek-flash\"\n",
    );
    user(
        &directory,
        "title",
        "---\ndescription: Titles\nmode: internal\nmodels: [\"${utility_model}\", \"${parent_model}\"]\n---\nName sessions.\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let utility = loaded.runtime.utility_model.expect("utility_model");
    assert_eq!(utility.model.to_string(), "deepseek/deepseek-flash");
    assert!(loaded.runtime.small_model.is_some());
    let models = &loaded.agents[&id("title")].frontmatter.models;
    assert_eq!(models[0].model, AgentModelRef::UtilityModel);
    assert_eq!(models[0].model.config_field(), Some("utility_model"));
    for config in [
        "utility_model = 3\n",
        "utility_model = \"not a model key\"\n",
    ] {
        assert!(
            load_from_user_root(Some(root(config).path())).is_err(),
            "{config:?} must be refused"
        );
    }
}
