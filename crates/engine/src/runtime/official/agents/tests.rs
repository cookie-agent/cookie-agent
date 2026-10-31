use std::{collections::BTreeMap, sync::Arc};

use sha2::{Digest as _, Sha256};

use crate::runtime::{
    official::skills::tests::{Routes, serve},
    tests::support::{Fixture, synthetic_default_fixture_with_config, write_private_test_file},
};

fn agent_md(description: &str) -> Vec<u8> {
    format!(
        "---\ndescription: {description}\nmode: primary\nmodels:\n  - custom.test/a-model\n---\nYou are {description}.\n"
    )
    .into_bytes()
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Publishes `agents` (id, version, requires_cookie, document) as the
/// source's index and files; the base URL `serve` gives ends in `/skills`.
fn publish(routes: &Routes, agents: &[(&str, &str, &str, Vec<u8>)]) {
    let mut routes = routes.lock().unwrap();
    routes.clear();
    let entries = agents
        .iter()
        .map(|(id, version, requires, body)| {
            routes.insert(format!("/skills/{id}.md"), body.clone());
            serde_json::json!({
                "id": id, "version": version, "requires_cookie": requires, "sha256": sha(body),
            })
        })
        .collect::<Vec<_>>();
    routes.insert(
        "/skills/index.json".into(),
        serde_json::to_vec(&serde_json::json!({"version": 1, "agents": entries})).unwrap(),
    );
}

fn fixture(extra_config: &str) -> Fixture {
    synthetic_default_fixture_with_config(None, "http://127.0.0.1:9/v1", extra_config)
        .expect("engine")
}

/// Where the configuration reads the official agents from.
fn official_root(fixture: &Fixture) -> std::path::PathBuf {
    fixture
        ._directory
        .path()
        .join(".cookie-agent/official_agents")
}

fn ids(report: &[String]) -> Vec<&str> {
    report.iter().map(String::as_str).collect()
}

fn listed(fixture: &Fixture, id: &str) -> Option<cookie_agent_protocol::AgentDescriptor> {
    fixture
        .engine
        .runtime_snapshot()
        .expect("snapshot")
        .snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == id && agent.preset.is_none())
        .cloned()
}

#[tokio::test]
async fn a_refresh_adds_changes_and_removes_agents_and_the_configuration_follows() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_agents_url(&url);
    let root = official_root(&fixture);

    publish(
        &routes,
        &[
            ("primary", "1.0.0", ">=0.1.0", agent_md("the assistant")),
            ("coder", "1.0.0", ">=0.1.0", agent_md("the coder")),
            ("future", "1.0.0", ">=99.0.0", agent_md("later")),
        ],
    );
    let report = fixture.engine.refresh_official_agents().await;
    assert!(report.enabled);
    assert_eq!(report.error, None);
    assert_eq!(ids(&report.updated), ["coder", "primary"]);
    assert!(root.join("primary.md").exists());
    assert!(
        !root.join("future.md").exists(),
        "an agent this version does not satisfy is not installed"
    );
    assert_eq!(
        *fixture.engine.official_agents(),
        BTreeMap::from([
            ("coder".to_owned(), "1.0.0".to_owned()),
            ("primary".to_owned(), "1.0.0".to_owned())
        ])
    );
    // The configuration reloaded: the agents are listed as official, and
    // the default agent can start a session.
    let primary = listed(&fixture, "primary").expect("primary is listed");
    assert!(primary.official);
    assert_eq!(primary.official_version.as_deref(), Some("1.0.0"));
    assert!(primary.runnable_as_root);
    assert_eq!(
        fixture
            .engine
            .runtime_snapshot()
            .expect("snapshot")
            .snapshot
            .default_agent
            .as_ref()
            .map(|id| id.as_str()),
        Some("primary")
    );

    // Nothing changed: nothing is reported.
    let again = fixture.engine.refresh_official_agents().await;
    assert!(again.updated.is_empty() && again.removed.is_empty());

    // primary changes, coder leaves the index.
    publish(
        &routes,
        &[(
            "primary",
            "1.1.0",
            ">=0.1.0",
            agent_md("the revised assistant"),
        )],
    );
    let report = fixture.engine.refresh_official_agents().await;
    assert_eq!(ids(&report.updated), ["primary"]);
    assert_eq!(ids(&report.removed), ["coder"]);
    assert!(!root.join("coder.md").exists());
    assert!(listed(&fixture, "coder").is_none());
    assert_eq!(
        listed(&fixture, "primary")
            .and_then(|agent| agent.official_version)
            .as_deref(),
        Some("1.1.0")
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_document_that_does_not_match_or_load_is_refused_and_the_cached_copy_stays() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_agents_url(&url);
    let root = official_root(&fixture);
    publish(
        &routes,
        &[("primary", "1.0.0", ">=0.1.0", agent_md("the assistant"))],
    );
    assert_eq!(fixture.engine.refresh_official_agents().await.error, None);

    // A new version whose file is not the one the index names.
    publish(
        &routes,
        &[("primary", "1.1.0", ">=0.1.0", agent_md("the new assistant"))],
    );
    routes
        .lock()
        .unwrap()
        .insert("/skills/primary.md".into(), agent_md("tampered"));
    let report = fixture.engine.refresh_official_agents().await;
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("does not match the index")),
        "{report:?}"
    );
    assert_eq!(
        std::fs::read(root.join("primary.md")).unwrap(),
        agent_md("the assistant")
    );

    // A document this build does not load: an unknown frontmatter key.
    let invalid = b"---\ndescription: New\nmode: primary\nfuture_key: 1\nmodels:\n  - custom.test/a-model\n---\nNew.\n".to_vec();
    publish(&routes, &[("primary", "2.0.0", ">=0.1.0", invalid)]);
    let report = fixture.engine.refresh_official_agents().await;
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("does not load")),
        "{report:?}"
    );
    assert_eq!(
        std::fs::read(root.join("primary.md")).unwrap(),
        agent_md("the assistant"),
        "the refused document never replaced the cached one"
    );
    assert_eq!(
        fixture
            .engine
            .official_agents()
            .get("primary")
            .map(String::as_str),
        Some("1.0.0")
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_user_document_of_the_same_id_replaces_the_official_one() {
    let fixture = fixture("");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_agents_url(&url);
    publish(
        &routes,
        &[("primary", "1.0.0", ">=0.1.0", agent_md("the assistant"))],
    );
    fixture.engine.refresh_official_agents().await;
    let agents = fixture._directory.path().join(".cookie-agent/agents");
    std::fs::create_dir_all(&agents).unwrap();
    write_private_test_file(&agents.join("primary.md"), agent_md("mine"));
    fixture.engine.reload_configuration().await.expect("reload");
    let primary = listed(&fixture, "primary").expect("primary");
    assert!(!primary.official, "the user's document wins whole");
    assert_eq!(primary.description, "mine");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn official_agents_off_refreshes_nothing_and_leaves_the_folder_unused() {
    let fixture = fixture("[agents]\nofficial = false\n");
    let root = official_root(&fixture);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("primary.md"), agent_md("kept from before")).unwrap();
    fixture.engine.reload_configuration().await.expect("reload");
    let report = fixture.engine.refresh_official_agents().await;
    assert!(!report.enabled);
    assert!(listed(&fixture, "primary").is_none());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_default_agent_names_whom_new_sessions_start_with() {
    let fixture = fixture("[agents]\ndefault = \"coder\"\n");
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    fixture.engine.set_official_agents_url(&url);
    publish(
        &routes,
        &[
            ("primary", "1.0.0", ">=0.1.0", agent_md("the assistant")),
            ("coder", "1.0.0", ">=0.1.0", agent_md("the coder")),
        ],
    );
    fixture.engine.refresh_official_agents().await;
    assert_eq!(
        fixture
            .engine
            .runtime_snapshot()
            .expect("snapshot")
            .snapshot
            .default_agent
            .as_ref()
            .map(|id| id.as_str()),
        Some("coder")
    );
    let status = fixture.engine.configuration_status();
    assert!(
        !status
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("default agent")),
        "{:?}",
        status.diagnostics
    );
    fixture.engine.shutdown().await;
}
