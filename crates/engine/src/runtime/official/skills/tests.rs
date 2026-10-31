use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use sha2::{Digest as _, Sha256};

use super::{UserSkillRoots, version_satisfies};
use crate::runtime::tests::support::{Fixture, synthetic_default_fixture_with_config};

pub(crate) type Routes = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

/// Serves `GET <path>` from `routes`, which the test may change; 404 for
/// anything else. The source URL.
pub(crate) async fn serve(routes: Routes) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let routes = Arc::clone(&routes);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let body = routes.lock().unwrap().get(&path).cloned();
                let response = match body {
                    Some(body) => {
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(&body);
                        response
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = socket.write_all(&response).await;
            });
        }
    });
    format!("http://{address}/skills")
}

fn skill_md(name: &str, description: &str) -> Vec<u8> {
    format!("---\nname: {name}\ndescription: {description}\n---\nDo {name}.\n").into_bytes()
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Publishes `skills` (name, version, requires_cookie, SKILL.md) as the
/// source's index and files.
fn publish(routes: &Routes, skills: &[(&str, &str, &str, Vec<u8>)]) {
    let mut routes = routes.lock().unwrap();
    routes.clear();
    let entries = skills
        .iter()
        .map(|(name, version, requires, body)| {
            routes.insert(format!("/skills/{name}/SKILL.md"), body.clone());
            serde_json::json!({
                "name": name, "version": version, "requires_cookie": requires,
                "files": [{"path": "SKILL.md", "sha256": sha(body)}],
            })
        })
        .collect::<Vec<_>>();
    routes.insert(
        "/skills/index.json".into(),
        serde_json::to_vec(&serde_json::json!({"version": 1, "skills": entries})).unwrap(),
    );
}

fn fixture(extra_config: &str) -> Fixture {
    synthetic_default_fixture_with_config(None, "http://127.0.0.1:9/v1", extra_config)
        .expect("engine")
}

fn official_root(fixture: &Fixture) -> std::path::PathBuf {
    fixture._directory.path().join("data/official_skills")
}

fn names(report: &[String]) -> Vec<&str> {
    report.iter().map(String::as_str).collect()
}

#[tokio::test]
async fn a_refresh_adds_changes_and_removes_skills_as_the_index_says() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_skills_url(&url);
    let root = official_root(&fixture);

    publish(
        &routes,
        &[
            ("alpha", "1.0.0", ">=0.1.0", skill_md("alpha", "First.")),
            ("beta", "1.0.0", ">=0.1.0", skill_md("beta", "Second.")),
            ("future", "1.0.0", ">=99.0.0", skill_md("future", "Later.")),
        ],
    );
    let report = fixture.engine.refresh_official_skills().await;
    assert!(report.enabled);
    assert_eq!(report.error, None);
    assert_eq!(names(&report.updated), ["alpha", "beta"]);
    assert!(root.join("alpha/SKILL.md").exists());
    assert!(
        !root.join("future").exists(),
        "a skill this version does not satisfy is not installed"
    );
    assert_eq!(
        *fixture.engine.official_skills(),
        BTreeMap::from([
            ("alpha".to_owned(), "1.0.0".to_owned()),
            ("beta".to_owned(), "1.0.0".to_owned())
        ])
    );

    // Nothing changed: nothing is reported.
    let again = fixture.engine.refresh_official_skills().await;
    assert!(again.updated.is_empty() && again.removed.is_empty());

    // alpha changes, beta leaves the index.
    publish(
        &routes,
        &[(
            "alpha",
            "1.1.0",
            ">=0.1.0",
            skill_md("alpha", "First, revised."),
        )],
    );
    let report = fixture.engine.refresh_official_skills().await;
    assert_eq!(names(&report.updated), ["alpha"]);
    assert_eq!(names(&report.removed), ["beta"]);
    assert!(!root.join("beta").exists());
    assert_eq!(
        std::fs::read(root.join("alpha/SKILL.md")).unwrap(),
        skill_md("alpha", "First, revised.")
    );

    // A requires_cookie this version no longer meets removes it.
    publish(
        &routes,
        &[("alpha", "2.0.0", ">=99.0.0", skill_md("alpha", "Later."))],
    );
    let report = fixture.engine.refresh_official_skills().await;
    assert_eq!(names(&report.removed), ["alpha"]);
    assert!(!root.join("alpha").exists());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_kept_versions_are_known_again_after_a_restart() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_skills_url(&url);
    publish(
        &routes,
        &[("alpha", "1.2.0", ">=0.1.0", skill_md("alpha", "First."))],
    );
    fixture.engine.refresh_official_skills().await;
    fixture.engine.shutdown().await;

    let reopened = crate::runtime::tests::support::reopen_engine(&fixture);
    assert_eq!(
        *reopened.official_skills(),
        BTreeMap::from([("alpha".to_owned(), "1.2.0".to_owned())])
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_file_that_does_not_match_the_index_is_refused_and_the_cache_stays() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_skills_url(&url);
    let root = official_root(&fixture);
    publish(
        &routes,
        &[("alpha", "1.0.0", ">=0.1.0", skill_md("alpha", "First."))],
    );
    fixture.engine.refresh_official_skills().await;

    publish(
        &routes,
        &[("alpha", "2.0.0", ">=0.1.0", skill_md("alpha", "Second."))],
    );
    routes
        .lock()
        .unwrap()
        .insert("/skills/alpha/SKILL.md".into(), b"tampered".to_vec());
    let report = fixture.engine.refresh_official_skills().await;
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("does not match the index")),
        "{report:?}"
    );
    assert_eq!(
        std::fs::read(root.join("alpha/SKILL.md")).unwrap(),
        skill_md("alpha", "First.")
    );
    assert_eq!(report.skills["alpha"], "1.0.0", "the cached version stays");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn offline_the_cached_skills_stay_and_the_failure_is_a_diagnostic() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_skills_url(&url);
    publish(
        &routes,
        &[("alpha", "1.0.0", ">=0.1.0", skill_md("alpha", "First."))],
    );
    fixture.engine.refresh_official_skills().await;

    // A source that answers nothing.
    fixture
        .engine
        .set_official_skills_url("http://127.0.0.1:9/skills");
    let report = fixture.engine.refresh_official_skills().await;
    assert!(report.error.is_some());
    assert_eq!(report.skills["alpha"], "1.0.0");
    assert!(official_root(&fixture).join("alpha/SKILL.md").exists());
    assert!(
        fixture
            .engine
            .configuration_status()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.starts_with("official skills were not refreshed")),
    );

    // Back online, the diagnostic clears.
    fixture.engine.set_official_skills_url(&url);
    assert_eq!(fixture.engine.refresh_official_skills().await.error, None);
    assert!(
        !fixture
            .engine
            .configuration_status()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.starts_with("official skills"))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn official_false_refreshes_nothing() {
    let fixture = fixture("\n[skills]\nofficial = false\n");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_skills_url(&url);
    publish(
        &routes,
        &[("alpha", "1.0.0", ">=0.1.0", skill_md("alpha", "First."))],
    );
    let report = fixture.engine.refresh_official_skills().await;
    assert!(!report.enabled);
    assert!(!official_root(&fixture).exists());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_refresh_offers_the_new_skills_below_the_user_s_own() {
    let fixture = fixture("");
    let user = fixture._directory.path().join("user-skills");
    std::fs::create_dir_all(user.join("beta")).unwrap();
    std::fs::write(
        user.join("beta/SKILL.md"),
        "---\nname: beta\ndescription: The user's own.\n---\nMine.\n",
    )
    .unwrap();
    fixture.engine.set_user_skill_roots(UserSkillRoots {
        shared: None,
        user: Some(user),
    });
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_skills_url(&url);
    publish(
        &routes,
        &[
            ("alpha", "1.2.0", ">=0.1.0", skill_md("alpha", "First.")),
            ("beta", "1.0.0", ">=0.1.0", skill_md("beta", "Official.")),
        ],
    );
    fixture.engine.refresh_official_skills().await;
    let listed = fixture.engine.list_daemon_skills().skills;
    let alpha = listed
        .iter()
        .find(|skill| skill.name == "alpha")
        .expect("alpha");
    assert_eq!(alpha.source, cookie_agent_protocol::SkillSource::Official);
    assert_eq!(alpha.version.as_deref(), Some("1.2.0"));
    let winner = listed
        .iter()
        .find(|skill| skill.name == "beta" && skill.precedence_winner)
        .expect("beta");
    assert_eq!(winner.source, cookie_agent_protocol::SkillSource::User);
    assert!(
        listed.iter().any(|skill| skill.name == "beta"
            && skill.source == cookie_agent_protocol::SkillSource::Official
            && !skill.precedence_winner),
        "the official beta is shadowed: {listed:?}"
    );
    fixture.engine.shutdown().await;
}

#[test]
fn version_ranges_hold_or_not() {
    assert_eq!(version_satisfies(">=0.3.0", "0.3.0"), Some(true));
    assert_eq!(version_satisfies(">=0.4.0", "0.3.9"), Some(false));
    assert_eq!(version_satisfies(">=0.3.0, <1.0.0", "0.9.1"), Some(true));
    assert_eq!(version_satisfies(">=0.3.0 <0.4", "0.4.0"), Some(false));
    assert_eq!(version_satisfies("=1.2.3", "1.2.3"), Some(true));
    assert_eq!(version_satisfies("1.2.3", "1.2.4"), Some(false));
    assert_eq!(version_satisfies("*", "0.0.1"), Some(true));
    // Pre-releases compare by semver precedence: nightlies are
    // 0.3.0-alpha.<n>, below 0.3.0.
    assert_eq!(
        version_satisfies(">=0.3.0-alpha.0", "0.3.0-alpha.1792798960"),
        Some(true)
    );
    assert_eq!(version_satisfies(">=0.3.0-alpha.0", "0.3.0"), Some(true));
    assert_eq!(
        version_satisfies(">=0.4.0", "0.3.0-alpha.1792798960"),
        Some(false)
    );
    assert_eq!(version_satisfies(">=0.4.0", "0.3.0"), Some(false));
    assert_eq!(version_satisfies(">=0.3.0", "0.3.0-alpha.1"), Some(false));
    assert_eq!(version_satisfies("<0.3.0", "0.3.0-alpha.1"), Some(true));
    assert_eq!(
        version_satisfies(">0.3.0-alpha.9", "0.3.0-alpha.10"),
        Some(true)
    );
    assert_eq!(
        version_satisfies(">0.3.0-alpha", "0.3.0-alpha.1"),
        Some(true)
    );
    assert_eq!(
        version_satisfies(">0.3.0-alpha.1", "0.3.0-beta"),
        Some(true)
    );
    assert_eq!(version_satisfies("=0.3.0+build.5", "0.3.0"), Some(true));
    assert_eq!(version_satisfies(">=0.3.0-", "0.3.0"), None);
    // Plain precedence, not Cargo's rule that a pre-release only matches a
    // comparator with a pre-release on the same version: a 0.4.0 nightly
    // satisfies a range written for 0.3.0 nightlies, and the daemon, whose
    // own version is its crate version, is a plain 0.4.0 either way.
    assert_eq!(
        version_satisfies(">=0.4.0-alpha.0", "0.4.0-alpha.1792900000"),
        Some(true)
    );
    assert_eq!(version_satisfies(">=0.4.0-alpha.0", "0.4.0"), Some(true));
    // A nightly of a later patch release.
    assert_eq!(
        version_satisfies(">=0.4.0-alpha.0", "0.4.1-alpha.0"),
        Some(true)
    );
    assert_eq!(
        version_satisfies(">=0.4.0-alpha.0", "0.3.0-alpha.1792798960"),
        Some(false)
    );
    assert_eq!(version_satisfies(">=0.4.0-alpha.0", "0.3.0"), Some(false));
    assert_eq!(
        version_satisfies(">=0.3.0-alpha.0", "0.4.0-alpha.1792900000"),
        Some(true)
    );
    assert_eq!(version_satisfies("~0.3", "0.3.0"), None);
    assert_eq!(version_satisfies("", "0.3.0"), None);
}

/// A new skill appears whole, replacing what an interrupted install left,
/// and a removed one is gone at once: a configuration load never finds a
/// skill directory without its `SKILL.md`. Hidden leftovers are cleaned.
#[test]
fn a_new_skill_appears_whole_and_a_removed_one_leaves_nothing_half_there() {
    let root = tempfile::tempdir().expect("root");
    let directory = root.path().join("alpha");
    std::fs::create_dir_all(directory.join("notes")).unwrap();
    std::fs::write(directory.join("notes/old.md"), "old").unwrap();
    std::fs::create_dir_all(root.path().join(".staging-alpha-1")).unwrap();
    std::fs::create_dir_all(root.path().join(".removing-beta-1")).unwrap();
    let files = vec![
        (directory.join("SKILL.md"), skill_md("alpha", "First.")),
        (directory.join("docs/guide.md"), b"guide".to_vec()),
    ];
    let listed = files.iter().map(|(path, _)| path.clone()).collect();
    assert!(super::install(&directory, files, &listed).expect("installed"));
    assert!(directory.join("SKILL.md").is_file());
    assert!(directory.join("docs/guide.md").is_file());
    assert!(
        !directory.join("notes").exists(),
        "the partial copy is replaced"
    );

    let (removed, written) = super::remove_others_and_record(
        root.path(),
        &[],
        &std::collections::BTreeSet::from(["alpha".to_owned()]),
    );
    assert_eq!(removed, ["alpha"]);
    written.expect("the kept index");
    let left = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(left, [super::KEPT_INDEX]);
}
