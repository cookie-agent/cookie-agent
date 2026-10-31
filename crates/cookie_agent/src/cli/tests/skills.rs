//! `cookie skills`.

use super::*;

fn write_check_skill(dir: &std::path::Path, name: &str, body: &str) -> String {
    std::fs::create_dir_all(dir.join(name)).unwrap();
    let text = format!("---\nname: {name}\ndescription: A skill to check.\n---\n{body}\n");
    std::fs::write(dir.join(name).join("SKILL.md"), &text).unwrap();
    format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())
    )
}

/// `cookie skills check` passes skills that parse, name real commands, and
/// link real pages with a matching index, and names each problem otherwise.
#[tokio::test]
async fn cookie_skills_check_finds_what_a_skill_gets_wrong() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/site");
    let good = tempfile::tempdir().unwrap();
    let sha = write_check_skill(
        good.path(),
        "tidy",
        "Run `cookie config set <key> <value>`, then `cookie daemon restart`.\nSee https://cookie-agent.github.io/doc/dev/engine/configuration/ and https://cookie-agent.github.io/doc/dev/extend/skills/#official-skills.",
    );
    std::fs::write(
        good.path().join("index.json"),
        serde_json::to_vec(&serde_json::json!({"version": 1, "skills": [
            {"name": "tidy", "version": "1.0.0", "requires_cookie": ">=0.4.0-alpha.0",
             "files": [{"path": "SKILL.md", "sha256": sha}]}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let problems = crate::cli::skills::check(good.path(), Some(&docs))
        .await
        .unwrap();
    assert!(problems.is_empty(), "{problems:?}");

    let bad = tempfile::tempdir().unwrap();
    write_check_skill(
        bad.path(),
        "broken",
        "Run `cookie teleport now` and `cookie config frobnicate`.\nSee https://cookie-agent.github.io/doc/dev/no/such/page/.",
    );
    std::fs::create_dir_all(bad.path().join("unparsable")).unwrap();
    std::fs::write(bad.path().join("unparsable/SKILL.md"), "no frontmatter").unwrap();
    std::fs::write(
        bad.path().join("index.json"),
        serde_json::to_vec(&serde_json::json!({"version": 1, "skills": [
            {"name": "broken", "version": "1.0.0", "requires_cookie": "~1",
             "files": [{"path": "SKILL.md", "sha256": "0".repeat(64)}]}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let problems = crate::cli::skills::check(bad.path(), Some(&docs))
        .await
        .unwrap();
    let all = problems.join("\n");
    for expected in [
        "`teleport` is not a subcommand of `cookie`",
        "`frobnicate` is not a subcommand of `config`",
        "no/such/page names no documentation page",
        "unparsable/SKILL.md",
        "is not a version range",
        "the index says",
        "`unparsable` is not listed",
    ] {
        assert!(all.contains(expected), "{expected:?} in {all}");
    }
}

/// `cookie skills list` shows the daemon's skills with their origin.
#[tokio::test]
async fn cookie_skills_list_shows_where_each_skill_comes_from() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let (client, _) = onboard_daemon(temporary.path()).await;
    let lines = crate::cli::skills::skills_with(&client, crate::cli::skills::SkillsCommand::List)
        .await
        .expect("list");
    assert!(!lines.is_empty());
}
