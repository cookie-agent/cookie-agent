//! `inherit:` in a user's agent document: the inherited document with this
//! one's keys over it (`docs/site/extend/agents.md`, Inheriting an agent).

use std::fs;

use cookie_agent_config::{ConfigError, PermissionKey, PermissionValue, load_from_user_root};
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

trait KeyText {
    fn key_text(&self) -> String;
}

impl KeyText for cookie_agent_config::AgentModelRef {
    fn key_text(&self) -> String {
        match self {
            Self::Model(key) => key.to_string(),
            other => format!("{other:?}"),
        }
    }
}

fn id(text: &str) -> AgentId {
    AgentId::new(text).unwrap()
}

const OFFICIAL_PRIMARY: &str = "---\ndescription: The assistant\nmode: primary\nmodels: [\"custom.test/default\"]\nlimits:\n  max_output_tokens: 1000\npermissions:\n  write:\n    \"${workspace_dir}/*\": allow\n    \"*\": ask\n  bash:\n    \"pwd\": allow\n    \"git status*\": allow\n    \"ls *\": allow\n    \"*\": ask\n  bash@labpc: deny\n  delegate:\n    explore: allow\n---\nBe the assistant.\n";

fn permission<'a>(
    document: &'a cookie_agent_config::AgentDocument,
    key: &str,
) -> Option<&'a PermissionValue> {
    document
        .frontmatter
        .permissions
        .get(&key.parse::<PermissionKey>().unwrap())
}

#[test]
fn an_inheriting_document_changes_its_keys_and_follows_the_official_agent_in_the_rest() {
    let directory = root();
    write(&directory, "official_agents/primary.md", OFFICIAL_PRIMARY);
    write(
        &directory,
        "agents/primary.md",
        "---\ninherit: ../official_agents/primary.md\nmodels: [\"custom.test/opus\"]\n---\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let primary = &loaded.agents[&id("primary")];
    assert!(
        !primary.official,
        "the user's document, extending the official one"
    );
    assert_eq!(primary.frontmatter.models.len(), 1);
    assert_eq!(
        primary.frontmatter.models[0].model.key_text(),
        "custom.test/opus"
    );
    assert_eq!(primary.frontmatter.description, "The assistant");
    assert_eq!(primary.frontmatter.limits.max_output_tokens, 1000);
    assert_eq!(primary.body, "Be the assistant.\n");
    assert!(permission(primary, "delegate").is_some());
    let before = primary.document_fingerprint.clone();

    // A refreshed official agent reaches the inheriting one.
    write(
        &directory,
        "official_agents/primary.md",
        &OFFICIAL_PRIMARY.replace("Be the assistant.", "Be a better assistant."),
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let primary = &loaded.agents[&id("primary")];
    assert_eq!(primary.body, "Be a better assistant.\n");
    assert_eq!(
        primary.frontmatter.models[0].model.key_text(),
        "custom.test/opus"
    );
    assert_ne!(primary.document_fingerprint, before);
}

#[test]
fn permissions_are_replaced_per_action_and_a_body_extends_the_inherited_prompt() {
    let directory = root();
    write(&directory, "official_agents/primary.md", OFFICIAL_PRIMARY);
    write(
        &directory,
        "agents/primary.md",
        "---\ninherit: ../official_agents/primary.md\npermissions:\n  bash: ask\n  message: allow\n---\n{{default_prompt}}\n\nAnswer in French.\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let primary = &loaded.agents[&id("primary")];
    // `bash` replaces both inherited `bash` keys, selector included.
    assert!(matches!(
        permission(primary, "bash"),
        Some(PermissionValue::Effect(
            cookie_agent_protocol::PermissionEffect::Ask
        ))
    ));
    assert!(permission(primary, "bash@labpc").is_none());
    // The rest stays as inherited; new actions are added.
    assert!(permission(primary, "write").is_some());
    assert!(permission(primary, "delegate").is_some());
    assert!(permission(primary, "message").is_some());
    assert_eq!(primary.body, "Be the assistant.\n\nAnswer in French.\n");
}

#[test]
fn any_agent_may_inherit_and_inheritance_chains() {
    let directory = root();
    write(&directory, "official_agents/primary.md", OFFICIAL_PRIMARY);
    write(
        &directory,
        "agents/primary.md",
        "---\ninherit: ../official_agents/primary.md\nmodels: [\"custom.test/opus\"]\n---\n{{default_prompt}}\nAnswer in French.\n",
    );
    // A second agent from the user's extended primary, on a faster model.
    write(
        &directory,
        "agents/fast.md",
        "---\ninherit: primary.md\nmodels: [\"custom.test/fast\"]\ndescription: The assistant, faster\n---\n",
    );
    // A preset's document from the top-level one.
    write(
        &directory,
        "agents/review/primary.md",
        "---\ninherit: ../primary.md\n---\n{{default_prompt}}\nReview only.\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let fast = &loaded.agents[&id("fast")];
    assert_eq!(
        fast.frontmatter.models[0].model.key_text(),
        "custom.test/fast"
    );
    assert_eq!(fast.frontmatter.description, "The assistant, faster");
    assert_eq!(fast.body, "Be the assistant.\nAnswer in French.\n");
    let review = &loaded.agent_presets["review"][&id("primary")];
    assert_eq!(
        review.frontmatter.models[0].model.key_text(),
        "custom.test/opus"
    );
    assert_eq!(
        review.body,
        "Be the assistant.\nAnswer in French.\nReview only.\n"
    );
}

#[test]
fn a_missing_target_leaves_the_agent_out_until_it_exists() {
    let directory = root();
    write(
        &directory,
        "agents/primary.md",
        "---\ninherit: ../official_agents/primary.md\nmodels: [\"custom.test/opus\"]\n---\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("never a load error");
    assert!(!loaded.agents.contains_key(&id("primary")));
    assert_eq!(loaded.official_agent_diagnostics.len(), 1);
    assert!(
        loaded.official_agent_diagnostics[0].contains("inherits `official_agents/primary.md`"),
        "{:?}",
        loaded.official_agent_diagnostics
    );
    // `[agents] official = false` leaves the official agents unused.
    write(&directory, "official_agents/primary.md", OFFICIAL_PRIMARY);
    fs::write(
        directory.path().join("config.toml"),
        "[agents]\nofficial = false\n",
    )
    .unwrap();
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert!(!loaded.agents.contains_key(&id("primary")));
}

#[test]
fn a_cycle_or_a_path_outside_the_agent_documents_is_refused() {
    let directory = root();
    write(&directory, "agents/a.md", "---\ninherit: b.md\n---\n");
    write(&directory, "agents/b.md", "---\ninherit: a.md\n---\n");
    assert!(matches!(
        load_from_user_root(Some(directory.path())),
        Err(ConfigError::AgentDocument { message, .. }) if message.contains("cycle")
    ));
    let directory = root();
    write(
        &directory,
        "agents/a.md",
        "---\ninherit: ../config.toml\n---\n",
    );
    assert!(matches!(
        load_from_user_root(Some(directory.path())),
        Err(ConfigError::AgentDocument { message, .. }) if message.contains("must name an agent document")
    ));
}

#[test]
fn an_official_agent_never_inherits() {
    let directory = root();
    write(&directory, "official_agents/primary.md", OFFICIAL_PRIMARY);
    write(
        &directory,
        "official_agents/coder.md",
        "---\ninherit: primary.md\n---\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("never a load error");
    assert!(loaded.agents.contains_key(&id("primary")));
    assert!(!loaded.agents.contains_key(&id("coder")));
    assert_eq!(
        loaded.official_agent_diagnostics.len(),
        1,
        "{:?}",
        loaded.official_agent_diagnostics
    );
}
