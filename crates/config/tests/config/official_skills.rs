//! `[skills] official` and the official skills folder as the lowest skill
//! root.

use std::fs;

use cookie_agent_config::{load_from_user_root, load_skill_roots_with_official};
use cookie_agent_protocol::SkillSource;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn official_takes_a_boolean() {
    let off = load_from_user_root(Some(root("[skills]\nofficial = false\n").path())).expect("off");
    assert!(!off.runtime.skills.official);
    let on = load_from_user_root(Some(root("[skills]\nofficial = true\n").path())).expect("on");
    assert!(on.runtime.skills.official);
}

#[test]
fn the_skills_table_is_strict() {
    for config in [
        "[skills]\nofficial = \"yes\"\n",
        "[skills]\nofficial = true\nsource = \"https://example.test\"\n",
        "skills = true\n",
    ] {
        assert!(
            load_from_user_root(Some(root(config).path())).is_err(),
            "{config:?} must be refused"
        );
    }
}

fn write_skill(root: &std::path::Path, name: &str, description: &str) {
    fs::create_dir_all(root.join(name)).expect("skill directory");
    fs::write(
        root.join(name).join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nDo it.\n"),
    )
    .expect("skill");
}

#[test]
fn official_skills_rank_below_user_skills_and_the_index_is_not_a_skill() {
    let directory = tempfile::tempdir().expect("tempdir");
    let official = directory.path().join("official_skills");
    let user = directory.path().join("skills");
    write_skill(&official, "shared-name", "Official.");
    write_skill(&official, "only-official", "Official only.");
    write_skill(&user, "shared-name", "The user's.");
    fs::write(official.join(".index.json"), "[]").expect("index");
    let registry =
        load_skill_roots_with_official(Some(&official), None, Some(&user), &[]).expect("skills");
    let shared = registry.get("shared-name").expect("shared-name");
    assert_eq!(shared.source, SkillSource::User);
    assert_eq!(shared.frontmatter.description, "The user's.");
    assert_eq!(
        registry.get("only-official").expect("official").source,
        SkillSource::Official
    );
    assert!(
        registry
            .discoveries()
            .iter()
            .any(|discovery| discovery.name == "shared-name"
                && discovery.source == SkillSource::Official
                && !discovery.precedence_winner)
    );
}
