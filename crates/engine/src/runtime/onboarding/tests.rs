use std::{collections::BTreeMap, path::Path};

use cookie_agent_config::{ConfigPath, ConfigTree, load_from_tree};
use sha2::{Digest as _, Sha256};

use super::{
    KitSource, OnboardingRequest, OnboardingRequirement,
    kit::{Kit, KitError, KitFileAction, KitPath, pack},
};
use crate::{
    EngineError,
    runtime::tests::support::{
        Fixture, create_private_test_dir, synthetic_default_fixture_with_config,
        write_private_test_file,
    },
};

/// The repository's `defaults/`, the kit the release packs.
fn defaults_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults")
}

fn defaults_files() -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("kit directory") {
            let path = entry.expect("kit entry").path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("below the kit")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, std::fs::read(&path).expect("kit file"));
            }
        }
    }
    let root = defaults_dir();
    let mut files = BTreeMap::new();
    walk(&root, &root, &mut files);
    files
}

#[test]
fn the_shipped_kit_matches_its_manifest_and_loads_with_this_build() {
    let kit = Kit::from_files(defaults_files()).unwrap_or_else(|error| {
        panic!(
            "defaults/ is not a valid kit ({error}); after changing a kit file run \
             scripts/pack-defaults.sh --update-manifest"
        )
    });
    let paths = kit.files.keys().map(KitPath::as_string).collect::<Vec<_>>();
    for expected in [
        "config.toml",
        "agents/primary.md",
        "agents/explore.md",
        "agents/worker.md",
        "skills/customize-cookie-agent/SKILL.md",
    ] {
        assert!(paths.iter().any(|path| path == expected), "{expected}");
    }
    assert!(
        kit.manifest.cookie_version.is_none(),
        "the release workflow writes cookie_version when it packs the kit"
    );
}

#[test]
fn the_kit_agents_are_the_root_and_its_two_subagents() {
    use cookie_agent_config::{AgentMode, AgentModelRef, PermissionKey};

    let kit = Kit::from_files(defaults_files()).expect("kit");
    let mut tree = ConfigTree::new(None);
    for (path, bytes) in kit.config_files() {
        tree.put(path.clone(), bytes.clone()).expect("put");
    }
    let loaded = load_from_tree(&tree).expect("kit loads");
    let agent = |id: &str| {
        &loaded.agents[&cookie_agent_identity::AgentId::new(id).expect("agent ID")].frontmatter
    };
    assert_eq!(agent("primary").mode, AgentMode::Primary);
    assert_eq!(agent("explore").mode, AgentMode::Subagent);
    assert_eq!(agent("worker").mode, AgentMode::Subagent);
    let models = |id: &str| {
        agent(id)
            .models
            .iter()
            .map(|entry| entry.model.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(models("primary"), [AgentModelRef::DefaultModel]);
    assert_eq!(
        models("explore"),
        [AgentModelRef::SmallModel, AgentModelRef::DefaultModel]
    );
    assert_eq!(models("worker"), [AgentModelRef::DefaultModel]);
    let write: PermissionKey = "write".parse().expect("key");
    assert!(
        !agent("explore").permissions.contains_key(&write),
        "explore never writes"
    );
}

#[test]
fn the_kit_config_examples_load_when_uncommented() {
    let text = String::from_utf8(defaults_files()["config.toml"].clone()).expect("UTF-8");
    let uncommented = text
        .lines()
        .filter(|line| !line.starts_with("##"))
        .map(|line| line.strip_prefix("# ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    let mut tree = ConfigTree::new(None);
    tree.put(ConfigPath::config(), uncommented.into_bytes())
        .expect("put");
    let loaded = load_from_tree(&tree).expect("every example is a valid setting");
    assert!(loaded.runtime.default_model.is_some());
    assert!(loaded.runtime.small_model.is_some());
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

const AGENT: &[u8] =
    b"---\ndescription: Helper\nmode: subagent\nmodels: [\"${small_model}\"]\n---\nHelp.\n";
const CONFIG: &[u8] = b"# a starter configuration\n";
const SKILL: &[u8] = b"---\nname: tidy\ndescription: Tidy things.\n---\nTidy.\n";
const MINE: &str = "---\ndescription: Mine\nmode: subagent\n---\nMine.\n";

fn manifest(files: &[(&str, &[u8])]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kit_version": 1,
        "cookie_version": "9.9.9",
        "files": files
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), sha(bytes)))
            .collect::<BTreeMap<_, _>>(),
    }))
    .expect("manifest")
}

fn archive() -> Vec<u8> {
    let files: [(&str, &[u8]); 3] = [
        ("config.toml", CONFIG),
        ("agents/helper.md", AGENT),
        ("skills/tidy/SKILL.md", SKILL),
    ];
    let manifest = manifest(&files);
    let mut entries = files.to_vec();
    entries.push(("manifest.json", &manifest));
    pack(&entries)
}

#[test]
fn an_archive_is_read_and_checked_against_its_manifest() {
    let kit = Kit::from_archive(&archive()).expect("kit");
    assert_eq!(kit.manifest.kit_version, 1);
    assert_eq!(kit.manifest.cookie_version.as_deref(), Some("9.9.9"));
    assert_eq!(kit.files.len(), 3);
    assert_eq!(kit.files[&KitPath::Skill("tidy".into())], SKILL);
}

#[test]
fn a_kit_that_does_not_match_its_manifest_is_refused() {
    let tampered = manifest(&[("config.toml", b"something else")]);
    let error = Kit::from_archive(&pack(&[
        ("config.toml", CONFIG),
        ("manifest.json", &tampered),
    ]))
    .expect_err("hash mismatch");
    assert!(matches!(error, KitError::HashMismatch { ref path, .. } if path == "config.toml"));

    let listed = manifest(&[("config.toml", CONFIG)]);
    assert!(matches!(
        Kit::from_archive(&pack(&[
            ("config.toml", CONFIG),
            ("agents/extra.md", AGENT),
            ("manifest.json", &listed),
        ])),
        Err(KitError::Unlisted(path)) if path == "agents/extra.md"
    ));
    let missing = manifest(&[("config.toml", CONFIG), ("agents/helper.md", AGENT)]);
    assert!(matches!(
        Kit::from_archive(&pack(&[
            ("config.toml", CONFIG),
            ("manifest.json", &missing)
        ])),
        Err(KitError::Missing(_))
    ));
    let escaping = manifest(&[("../config.toml", CONFIG)]);
    assert!(matches!(
        Kit::from_archive(&pack(&[
            ("../config.toml", CONFIG),
            ("manifest.json", &escaping)
        ])),
        Err(KitError::UnsafePath(_))
    ));
    let unknown_file = manifest(&[("notes.txt", CONFIG)]);
    assert!(matches!(
        Kit::from_archive(&pack(&[
            ("notes.txt", CONFIG),
            ("manifest.json", &unknown_file)
        ])),
        Err(KitError::UnsafePath(_))
    ));
    assert!(matches!(
        Kit::from_archive(b"not gzip"),
        Err(KitError::Archive(_))
    ));
}

#[test]
fn a_kit_this_build_cannot_load_is_refused() {
    let config = b"unknown_key = 1\n";
    let invalid = manifest(&[("config.toml", config)]);
    assert!(matches!(
        Kit::from_archive(&pack(&[
            ("config.toml", config),
            ("manifest.json", &invalid)
        ])),
        Err(KitError::Invalid(_))
    ));
    let skill = b"---\nname: other\ndescription: Named for another directory.\n---\nNo.\n";
    let mismatched = manifest(&[("skills/tidy/SKILL.md", skill)]);
    assert!(matches!(
        Kit::from_archive(&pack(&[
            ("skills/tidy/SKILL.md", skill),
            ("manifest.json", &mismatched)
        ])),
        Err(KitError::Invalid(_))
    ));
}

#[test]
fn the_plan_writes_new_files_skips_identical_ones_and_puts_changed_ones_beside() {
    let kit = Kit::from_archive(&archive()).expect("kit");
    let root = tempfile::tempdir().expect("root");
    let empty = ConfigTree::read(Some(root.path())).expect("tree");
    assert!(
        kit.plan(&empty, Some(root.path()))
            .iter()
            .all(|file| file.action == KitFileAction::Write)
    );

    std::fs::write(root.path().join("config.toml"), CONFIG).expect("config");
    std::fs::create_dir_all(root.path().join("agents")).expect("agents");
    std::fs::write(root.path().join("agents/helper.md"), MINE).expect("agent");
    std::fs::create_dir_all(root.path().join("skills/tidy")).expect("skills");
    std::fs::write(root.path().join("skills/tidy/SKILL.md"), SKILL).expect("skill");
    let tree = ConfigTree::read(Some(root.path())).expect("tree");
    let plan = kit.plan(&tree, Some(root.path()));
    let file = |path: &str| {
        plan.iter()
            .find(|file| file.path.as_string() == path)
            .unwrap_or_else(|| panic!("{path}"))
    };
    assert_eq!(file("config.toml").action, KitFileAction::Skip);
    assert_eq!(file("skills/tidy/SKILL.md").action, KitFileAction::Skip);
    let changed = file("agents/helper.md");
    assert_eq!(changed.action, KitFileAction::WriteNew);
    let diff = changed.diff.as_deref().expect("diff");
    assert!(diff.contains("-description: Mine"), "{diff}");
    assert!(diff.contains("+description: Helper"), "{diff}");
    assert!(diff.contains("agents/helper.md.new"), "{diff}");
}

/// The synthetic fixture: provider `custom.test` with `a-model` and
/// `z-model` in its `config.toml`, and no agents.
fn fixture() -> Fixture {
    synthetic_default_fixture_with_config(None, "http://127.0.0.1:9/v1", "").expect("engine")
}

fn root(fixture: &Fixture) -> std::path::PathBuf {
    fixture._directory.path().join(".cookie-agent")
}

fn ready_request(install_kit: bool, source: KitSource) -> OnboardingRequest {
    OnboardingRequest {
        install_kit,
        source,
        default_model: Some(
            serde_json::json!({"model": "custom.test/a-model", "variant": "alpha"}),
        ),
        small_model: Some(serde_json::json!("custom.test/z-model")),
    }
}

#[tokio::test]
async fn readiness_follows_the_configuration_and_the_providers() {
    let fixture = fixture();
    let state = fixture.engine.onboarding_state();
    assert_eq!(
        state.missing,
        [
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::SmallModel
        ]
    );
    assert_eq!(state.providers_connected.len(), 1);

    // Set up by hand: config.toml edited and reloaded counts the same.
    let config = root(&fixture).join("config.toml");
    let text = std::fs::read_to_string(&config).expect("config");
    write_private_test_file(
        &config,
        format!(
            "default_model = \"custom.test/a-model\"\nsmall_model = \"custom.test/a-model\"\n{text}"
        ),
    );
    fixture.engine.reload_configuration().await.expect("reload");
    let state = fixture.engine.onboarding_state();
    assert!(state.ready(), "{state:?}");

    // A model that is gone, then no provider at all.
    write_private_test_file(
        &config,
        format!(
            "default_model = \"custom.test/gone\"\nsmall_model = \"custom.test/a-model\"\n{text}"
        ),
    );
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        fixture.engine.onboarding_state().missing,
        [OnboardingRequirement::DefaultModel]
    );
    write_private_test_file(
        &config,
        "default_model = \"custom.test/a-model\"\nsmall_model = \"custom.test/a-model\"\n",
    );
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        fixture.engine.onboarding_state().missing,
        [
            OnboardingRequirement::Provider,
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::SmallModel
        ]
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_run_that_would_not_be_ready_writes_nothing() {
    let fixture = fixture();
    let before = std::fs::read(root(&fixture).join("config.toml")).expect("config");
    let request = OnboardingRequest {
        small_model: None,
        ..ready_request(true, KitSource::Archive(archive()))
    };
    let error = fixture
        .engine
        .run_onboarding(request)
        .await
        .expect_err("small_model is missing");
    assert!(matches!(
        error,
        EngineError::OnboardingIncomplete(ref missing)
            if missing == &[OnboardingRequirement::SmallModel]
    ));
    let unavailable = OnboardingRequest {
        default_model: Some(serde_json::json!("custom.test/gone")),
        ..ready_request(true, KitSource::Archive(archive()))
    };
    assert!(matches!(
        fixture.engine.run_onboarding(unavailable).await,
        Err(EngineError::OnboardingIncomplete(_))
    ));
    assert_eq!(
        std::fs::read(root(&fixture).join("config.toml")).expect("config"),
        before
    );
    assert!(!root(&fixture).join("agents").exists());
    assert!(!root(&fixture).join("skills").exists());
    assert!(!root(&fixture).join("config.toml.new").exists());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_run_sets_the_models_and_installs_the_kit_beside_changed_files() {
    let fixture = fixture();
    let agents = root(&fixture).join("agents");
    create_private_test_dir(&agents);
    write_private_test_file(&agents.join("helper.md"), MINE);
    fixture.engine.reload_configuration().await.expect("reload");

    let preview = fixture
        .engine
        .preview_kit(KitSource::Archive(archive()))
        .await
        .expect("preview");
    assert_eq!(preview.origin, "archive");
    let actions = preview
        .files
        .iter()
        .map(|file| (file.path.as_string(), file.action))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actions["agents/helper.md"], KitFileAction::WriteNew);
    assert_eq!(actions["config.toml"], KitFileAction::WriteNew);
    assert_eq!(actions["skills/tidy/SKILL.md"], KitFileAction::Write);

    let outcome = fixture
        .engine
        .run_onboarding(ready_request(true, KitSource::Archive(archive())))
        .await
        .expect("run");
    assert!(outcome.state.ready());
    assert_eq!(outcome.settings, ["default_model", "small_model"]);
    assert_eq!(outcome.written, ["skills/tidy/SKILL.md"]);
    assert_eq!(outcome.written_new, ["agents/helper.md", "config.toml"]);
    assert_eq!(
        std::fs::read(agents.join("helper.md.new")).expect(".new"),
        AGENT
    );
    assert_eq!(
        std::fs::read_to_string(agents.join("helper.md")).expect("kept"),
        MINE
    );
    assert_eq!(
        std::fs::read(root(&fixture).join("skills/tidy/SKILL.md")).expect("skill"),
        SKILL
    );
    let config = fixture.engine.configuration();
    assert_eq!(
        config
            .runtime
            .small_model
            .as_ref()
            .map(|model| model.model.to_string()),
        Some("custom.test/z-model".into())
    );

    // Again: the skill is identical now and nothing else changes.
    let again = fixture
        .engine
        .run_onboarding(OnboardingRequest {
            default_model: None,
            small_model: None,
            ..ready_request(true, KitSource::Archive(archive()))
        })
        .await
        .expect("second run");
    assert_eq!(again.skipped, ["skills/tidy/SKILL.md"]);
    assert!(again.written.is_empty() && again.settings.is_empty());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn new_kit_documents_are_written_and_applied() {
    let fixture = fixture();
    let config = root(&fixture).join("config.toml");
    let outcome = fixture
        .engine
        .run_onboarding(ready_request(true, KitSource::Archive(archive())))
        .await
        .expect("run");
    assert_eq!(
        outcome.written,
        ["agents/helper.md", "skills/tidy/SKILL.md"]
    );
    assert_eq!(
        outcome.written_new,
        ["config.toml"],
        "the fixture's own config.toml stays"
    );
    let written = std::fs::read_to_string(&config).expect("config");
    assert!(written.contains("default_model"));
    assert!(written.contains("custom.test"), "{written}");
    assert!(
        fixture
            .engine
            .runtime_snapshot()
            .expect("runtime")
            .snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "helper"),
        "the configuration was applied"
    );
    fixture.engine.shutdown().await;
}

/// Serves `GET <path>` from `routes`, 404 for anything else; the base URL
/// of `/releases`.
async fn serve(routes: BTreeMap<String, Vec<u8>>) -> String {
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
            let routes = routes.clone();
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
                let response = match routes.get(&path) {
                    Some(body) => {
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/gzip\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(body);
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
    format!("http://{address}/releases")
}

#[tokio::test]
async fn the_release_source_falls_back_to_the_latest_release() {
    let fixture = fixture();
    let base = serve(BTreeMap::from([(
        "/releases/latest/download/cookie-agent-defaults.tar.gz".to_owned(),
        archive(),
    )]))
    .await;
    fixture.engine.set_releases_url(&base);
    let preview = fixture
        .engine
        .preview_kit(KitSource::Release)
        .await
        .expect("preview from the latest release");
    assert_eq!(
        preview.origin,
        format!("{base}/latest/download/cookie-agent-defaults.tar.gz")
    );

    let versioned = serve(BTreeMap::from([(
        format!(
            "/releases/download/v{}/cookie-agent-defaults.tar.gz",
            env!("CARGO_PKG_VERSION")
        ),
        archive(),
    )]))
    .await;
    fixture.engine.set_releases_url(&versioned);
    let preview = fixture
        .engine
        .preview_kit(KitSource::Release)
        .await
        .expect("preview from this version's release");
    assert!(preview.origin.contains(env!("CARGO_PKG_VERSION")));

    let empty = serve(BTreeMap::new()).await;
    fixture.engine.set_releases_url(&empty);
    assert!(matches!(
        fixture.engine.preview_kit(KitSource::Release).await,
        Err(EngineError::OnboardingKit(KitError::Unavailable(_)))
    ));
    fixture.engine.shutdown().await;
}

#[test]
fn a_gnu_tar_archive_of_the_kit_reads() {
    // The release packs the kit with GNU tar (scripts/pack-defaults.sh);
    // check that real archive when tar is available.
    let Ok(output) = std::process::Command::new("tar").arg("--version").output() else {
        return;
    };
    if !String::from_utf8_lossy(&output.stdout).contains("GNU tar") {
        return;
    }
    let directory = tempfile::tempdir().expect("tempdir");
    let archive = directory.path().join("kit.tar.gz");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/pack-defaults.sh");
    let status = std::process::Command::new("bash")
        .arg(script)
        .arg("v0.0.0-test")
        .arg(&archive)
        .status()
        .expect("pack-defaults.sh");
    assert!(status.success());
    let kit = Kit::from_archive(&std::fs::read(&archive).expect("archive")).expect("kit");
    assert_eq!(kit.manifest.cookie_version.as_deref(), Some("0.0.0-test"));
    assert_eq!(kit.files.len(), defaults_files().len() - 1);
}
