use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_files(root, &path, files);
        } else {
            files.push(path.strip_prefix(root).unwrap().to_owned());
        }
    }
}

const WORKSPACE_MANIFESTS: &[&str] = &[
    "crates/identity/Cargo.toml",
    "crates/config/Cargo.toml",
    "crates/cookie_agent/Cargo.toml",
    "crates/engine/Cargo.toml",
    "crates/models/Cargo.toml",
    "crates/plugin_sdk/Cargo.toml",
    "crates/protocol/Cargo.toml",
    "crates/server/Cargo.toml",
    "crates/tools/Cargo.toml",
    "crates/tui/Cargo.toml",
];
const PHASE1_MANIFESTS: &[&str] = &[
    "crates/identity/Cargo.toml",
    "crates/config/Cargo.toml",
    "crates/models/Cargo.toml",
];
const PUBLISHED_CRATES: &[&str] = &[
    "cookie_agent_identity",
    "cookie_agent_plugin_sdk",
    "cookie_agent_protocol",
];

fn cargo_metadata() -> serde_json::Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(workspace())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn secret_markers() -> Vec<Vec<u8>> {
    vec![
        [b"-----BEGIN ".as_slice(), b"PRIVATE KEY-----"].concat(),
        [b"-----BEGIN RSA ".as_slice(), b"PRIVATE KEY-----"].concat(),
        [b"-----BEGIN EC ".as_slice(), b"PRIVATE KEY-----"].concat(),
        [b"-----BEGIN OPENSSH ".as_slice(), b"PRIVATE KEY-----"].concat(),
        [b"sk-".as_slice(), b"proj-"].concat(),
        [b"sk-ant-".as_slice(), b"api03-"].concat(),
        [b"github_".as_slice(), b"pat_"].concat(),
        [b"gh".as_slice(), b"p_"].concat(),
        [b"xo".as_slice(), b"xb-"].concat(),
        [b"AIza".as_slice(), b"Sy"].concat(),
    ]
}

fn assert_no_secret_material(path: &Path, bytes: &[u8]) {
    for marker in secret_markers() {
        assert!(
            !bytes.windows(marker.len()).any(|window| window == marker),
            "secret marker found in {}",
            path.display()
        );
    }
    assert!(
        !bytes.windows(20).any(|window| {
            window.starts_with(b"AKIA")
                && window[4..]
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        }),
        "AWS access-key-shaped material found in {}",
        path.display()
    );
}

fn owned_source_files() -> Vec<PathBuf> {
    let root = workspace();
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git source listing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8(path.to_vec()).unwrap()))
        .filter(|path| !path.starts_with("target") && !path.starts_with("assets"))
        .map(|path| root.join(path))
        .filter(|path| path.is_file())
        .collect()
}

#[test]
fn oven_dependencies_use_one_pinned_git_revision_with_exact_publish_versions() {
    let manifest = fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    for pin in [
        "oven-sdk = { version = \"=0.6.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-anthropic = { version = \"=0.7.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-openai = { version = \"=0.6.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-google = { version = \"=0.6.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-google-vertex = { version = \"=0.6.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-bedrock = { version = \"=0.5.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-azure = { version = \"=0.5.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
        "oven-sdk-cohere = { version = \"=0.4.0\", git = \"https://github.com/cookie-agent/oven-sdk.git\", rev = \"7d4e68607e7643c4aa8fb46bf26f58a02832efa8\" }",
    ] {
        assert!(manifest.contains(pin), "missing exact pin: {pin}");
    }
}

#[test]
fn models_source_has_no_unapproved_open_responses_adapter_surface() {
    let models = workspace().join("crates/models");
    let source = models.join("src");
    let mut files = Vec::new();
    collect_files(&source, &source, &mut files);
    let markers = [
        ["Open", "Responses"].concat(),
        ["open", "_responses"].concat(),
        ["open", "-responses"].concat(),
        ["protocol", "_mode"].concat(),
    ];
    for relative in files {
        let path = source.join(relative);
        let text = fs::read_to_string(&path).unwrap();
        for marker in &markers {
            assert!(
                !text.contains(marker),
                "future Open Responses marker `{marker}` remains in {}",
                path.display()
            );
        }
    }
    let manifest = fs::read_to_string(models.join("Cargo.toml")).unwrap();
    assert!(!manifest.contains(&["oven-sdk-open", "-responses"].concat()));
}

#[test]
fn the_wire_contract_ships_no_typescript_bindings_or_checked_in_generated_tree() {
    let workspace = workspace();
    let manifest = fs::read_to_string(workspace.join("Cargo.toml")).unwrap();
    assert!(!manifest.contains("ts-rs"), "ts-rs must not be declared");
    for absent in [
        "crates/protocol/generated",
        "crates/protocol/typescript",
        "crates/protocol/scripts/check-bindings.sh",
    ] {
        assert!(
            !workspace.join(absent).exists(),
            "removed binding artifact remains: {absent}"
        );
    }
    let script = workspace.join("crates/protocol/scripts/check-schema-additive.sh");
    let text = fs::read_to_string(&script).unwrap();
    for forbidden in ["npm", "node_modules", "tsc"] {
        assert!(
            !text.contains(forbidden),
            "the additive schema check must not need a JavaScript toolchain: {forbidden}"
        );
    }
    for manifest in ["crates/protocol/Cargo.toml", "crates/identity/Cargo.toml"] {
        assert!(
            !fs::read_to_string(workspace.join(manifest))
                .unwrap()
                .contains("ts-rs"),
            "{manifest} must not depend on ts-rs"
        );
    }
}

#[test]
fn syntect_is_exactly_pinned_from_crates_io() {
    let manifest = fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("syntect = { version = \"=5.3.0\""));
    assert!(
        !manifest.contains("[patch.crates-io]"),
        "no dependency may be patched to a local path"
    );
    let root = manifest.parse::<toml::Value>().unwrap();
    let syntect = root["workspace"]["dependencies"]["syntect"]
        .as_table()
        .unwrap();
    assert_eq!(syntect["version"].as_str(), Some("=5.3.0"));
    assert_eq!(syntect["default-features"].as_bool(), Some(false));
    assert!(
        syntect.get("path").is_none() && syntect.get("git").is_none(),
        "syntect must resolve from crates.io"
    );
    assert_eq!(
        syntect["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|feature| feature.as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["default-syntaxes", "default-themes", "regex-fancy"])
    );
    assert!(
        !workspace().join("vendor").exists(),
        "the vendored dependency tree is gone"
    );

    let lock = fs::read_to_string(workspace().join("Cargo.lock")).unwrap();
    assert!(lock.contains("name = \"bincode\"\n"));
    assert!(lock.contains(
        "checksum = \"656b45c05d95a5704399aeef6bd0ddec7b2b3531b7c9e900abbf7c4d2190c925\""
    ));
}

#[test]
fn every_internal_path_dependency_has_its_exact_package_version() {
    for manifest in PHASE1_MANIFESTS.iter().copied() {
        let manifest_path = workspace().join(manifest);
        let document = fs::read_to_string(&manifest_path)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(dependencies) = document.get(section).and_then(toml::Value::as_table) else {
                continue;
            };
            for (name, dependency) in dependencies {
                let Some(dependency) = dependency.as_table() else {
                    continue;
                };
                let Some(path) = dependency.get("path").and_then(toml::Value::as_str) else {
                    continue;
                };
                let target_manifest = manifest_path
                    .parent()
                    .unwrap()
                    .join(path)
                    .join("Cargo.toml");
                let target = fs::read_to_string(&target_manifest)
                    .unwrap()
                    .parse::<toml::Value>()
                    .unwrap();
                let target_version = target["package"]["version"].as_str().unwrap();
                let expected = format!("={target_version}");
                assert_eq!(
                    dependency.get("version").and_then(toml::Value::as_str),
                    Some(expected.as_str()),
                    "{manifest} path dependency {name} must exactly pin {target_version}"
                );
            }
        }
    }
}

#[test]
fn workspace_metadata_limits_publishing_and_uses_registry_syntect() {
    let metadata = cargo_metadata();
    let members = metadata["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member.as_str().unwrap())
        .collect::<BTreeSet<_>>();
    let packages = metadata["packages"].as_array().unwrap();
    let workspace_packages = packages
        .iter()
        .filter(|package| members.contains(package["id"].as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(workspace_packages.len(), WORKSPACE_MANIFESTS.len());
    for package in &workspace_packages {
        let name = package["name"].as_str().unwrap();
        if PUBLISHED_CRATES.contains(&name) {
            assert!(package["publish"].is_null(), "{name} must be publishable");
        } else {
            assert_eq!(
                package["publish"].as_array().map(Vec::len),
                Some(0),
                "{name} must remain nonpublishable"
            );
        }
    }
    assert_eq!(
        workspace_packages
            .iter()
            .filter(|package| package["name"]
                .as_str()
                .unwrap()
                .starts_with("cookie_agent"))
            .count(),
        WORKSPACE_MANIFESTS.len()
    );
    let syntect_package = packages
        .iter()
        .find(|package| package["name"] == "syntect")
        .unwrap();
    assert_eq!(syntect_package["version"].as_str(), Some("5.3.0"));
    assert!(syntect_package["source"].as_str().is_some_and(|source| {
        source.starts_with("registry+https://github.com/rust-lang/crates.io-index")
    }));
    let bincode_package = packages
        .iter()
        .find(|package| package["name"] == "bincode")
        .unwrap();
    assert_eq!(bincode_package["version"].as_str(), Some("1.3.3"));
    assert!(bincode_package["source"].as_str().is_some_and(|source| {
        source.starts_with("registry+https://github.com/rust-lang/crates.io-index")
    }));
    let syntect_node = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == syntect_package["id"])
        .unwrap();
    assert_eq!(
        syntect_node["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|feature| feature.as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "bincode",
            "default-syntaxes",
            "default-themes",
            "dump-create",
            "dump-load",
            "fancy-regex",
            "flate2",
            "fnv",
            "parsing",
            "regex-fancy",
            "regex-syntax",
        ])
    );
}

#[test]
fn root_lockfile_is_the_only_checked_in_dependency_graph() {
    fn collect_lockfiles(root: &Path, directory: &Path, lockfiles: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".git" | "target")
                ) {
                    continue;
                }
                collect_lockfiles(root, &path, lockfiles);
            } else if path.file_name().is_some_and(|name| name == "Cargo.lock") {
                lockfiles.push(path.strip_prefix(root).unwrap().to_owned());
            }
        }
    }

    let root = workspace();
    let mut lockfiles = Vec::new();
    collect_lockfiles(&root, &root, &mut lockfiles);
    lockfiles.sort();
    assert_eq!(lockfiles, [PathBuf::from("Cargo.lock")]);
}

#[test]
fn owned_source_tree_contains_no_secret_or_temporary_material() {
    let root = workspace();
    for path in owned_source_files() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        assert!(
            !matches!(
                name,
                ".env" | ".env.local" | "credentials.json" | "secrets.json"
            ) && !name.ends_with(".pending-snap")
                && !name.ends_with(".pem")
                && !name.ends_with(".p12")
                && !name.ends_with(".key"),
            "secret or temporary source asset present: {}",
            path.display()
        );
        assert_no_secret_material(&path, &fs::read(&path).unwrap());
    }
    assert!(
        !root.join("target/package").exists(),
        "target/package is not a supported release output"
    );
}

#[test]
#[ignore = "requires the locked release binary built by the release gate"]
fn release_binary_contains_no_secret_material() {
    let path = workspace().join("target/release/cookie");
    assert!(
        path.is_file(),
        "missing locked release binary: {}",
        path.display()
    );
    assert_no_secret_material(&path, &fs::read(&path).unwrap());
}

#[test]
fn ci_supply_chain_and_release_gates_are_pinned() {
    let workflow = fs::read_to_string(workspace().join(".github/workflows/ci.yml")).unwrap();

    for line in workflow.lines() {
        let Some((_, reference)) = line.trim().split_once("uses: ") else {
            continue;
        };
        let reference = reference.split_whitespace().next().unwrap();
        if reference.starts_with("./") {
            continue;
        }
        let (action, revision) = reference.rsplit_once('@').unwrap_or((reference, ""));
        assert!(
            revision.len() == 40 && revision.chars().all(|byte| byte.is_ascii_hexdigit()),
            "{action} must be pinned by a full commit SHA, found `{revision}`"
        );
    }

    for required in [
        "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683",
        "dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4",
        "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
        "taiki-e/install-action@94c31af3204a9f15ab40b35ad084410b905bbc73",
        "tool: cargo-deny@0.20.2,cargo-audit@0.22.2",
        "cargo clippy --locked --workspace --all-targets -- -D warnings",
        "cargo build --release --locked -p cookie_agent",
        "cargo audit --file Cargo.lock --deny yanked",
        "cargo deny --locked check advisories licenses sources",
        "cargo test --locked -p cookie_agent_models --test release_integrity",
        "crates/protocol/scripts/check-schema-additive.sh",
        "release_binary_contains_no_secret_material -- --ignored --exact",
        "needs: [stable, msrv, windows, release-targets]",
    ] {
        assert!(
            workflow.contains(required),
            "missing pinned CI gate: {required}"
        );
    }

    for forbidden in [
        ["RUSTC", "_BOOTSTRAP"].concat(),
        ["--allow", "-dirty"].concat(),
        ["--no", "-verify"].concat(),
        ["package", "_workspace.sh"].concat(),
        ["cargo install --locked cargo-", "audit"].concat(),
        ["cargo install --locked cargo-", "deny"].concat(),
        [
            "cargo build --release --locked --workspace",
            " --all-targets",
        ]
        .concat(),
    ] {
        assert!(
            !workflow.contains(&forbidden),
            "forbidden packaging bypass remains in CI: {forbidden}"
        );
    }
}

#[test]
fn model_package_catalog_network_is_fixed_and_has_no_unapproved_adapters() {
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let current_facade_source = [
        "lib.rs",
        "model_types.rs",
        "manager/mod.rs",
        "manifests/mod.rs",
    ]
    .into_iter()
    .map(|file| fs::read_to_string(source_root.join(file)).unwrap())
    .collect::<String>();
    for forbidden in [
        "ModelRegistry",
        "MiniMaxModel",
        "AnthropicAwsModel",
        "MINIMAX_PROVIDER_ID",
        "ANTHROPIC_AWS_PROVIDER_ID",
        "starts_with(\"gpt",
        "starts_with(\"claude",
        "replay_discriminator",
        "google_vertex_replay_scope",
    ] {
        assert!(
            !current_facade_source.contains(forbidden),
            "forbidden model-package behavior: {forbidden}"
        );
    }

    let catalog_root = source_root.join("catalog");
    let catalog_source = fs::read_dir(&catalog_root)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(catalog_source.contains("https://models.dev/catalog.json"));
    for forbidden in [
        "reqwest::get",
        "Policy::limited",
        "Accept-Encoding: gzip",
        "catalog_url",
        "MODELS_DEV_LIVE_SHA256",
        "std::env::var",
        "std::env::var_os",
    ] {
        assert!(
            !catalog_source.contains(forbidden),
            "forbidden dynamic catalog behavior: {forbidden}"
        );
    }
    assert!(catalog_source.contains("Policy::none"));
    assert!(catalog_source.contains("accept_encoding: \"identity\""));
    assert!(catalog_source.contains("connect_timeout(Duration::from_secs(5))"));
    assert!(catalog_source.contains("timeout(Duration::from_secs(15))"));

    let manifest = fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    assert!(manifest.contains(
        "reqwest = { version = \"=0.13.4\", default-features = false, features = [\"json\", \"stream\", \"rustls\"] }"
    ));
    assert!(
        !manifest.contains("reqwest-oven"),
        "the workspace must depend on a single reqwest"
    );
}

#[test]
fn synthetic_metadata_fixture_is_explicitly_unapproved_safe_and_not_a_runtime_pin() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models-dev-metadata-synthetic.json");
    let metadata_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models-dev-metadata-synthetic.meta.json");
    let bytes = fs::read(&fixture).unwrap();
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    assert_eq!(metadata["byte_length"], bytes.len() as u64);
    assert_eq!(metadata["sha256"], format!("{:x}", Sha256::digest(&bytes)));
    assert_eq!(metadata["runtime_pin"], false);
    assert_eq!(metadata["contains_secrets"], false);
    assert_eq!(metadata["approved_live_audit"], false);
    assert_eq!(metadata["fixture_kind"], "invented_metadata_edge_cases");
    assert_no_secret_material(&fixture, &bytes);
}

#[test]
fn approved_full_live_catalog_fixture_has_exact_capture_integrity() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let fixture = root.join("models-dev-live-audit-2026-08-05.json");
    let metadata_path = root.join("models-dev-live-audit-2026-08-05.meta.json");
    let bytes = fs::read(&fixture).unwrap();
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    assert_eq!(bytes.len(), 3_801_566);
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "25dd5dd6eb21b2d78044606eeb806d8cdd38640c8deea071122d5591edb88795"
    );
    assert_eq!(metadata["schema_version"], 1);
    assert_eq!(metadata["fixture_kind"], "full_live_catalog_audit");
    assert_eq!(metadata["review_status"], "approved");
    assert_eq!(metadata["source_url"], "https://models.dev/catalog.json");
    assert_eq!(metadata["captured_at"], "2026-08-05T22:11:05Z");
    assert_eq!(metadata["etag"], "\"25dd5dd6eb21b2d78044606eeb806d8c\"");
    assert_eq!(metadata["accept_encoding"], "identity");
    assert_eq!(metadata["byte_length"], 3_801_566);
    assert_eq!(
        metadata["sha256"],
        "25dd5dd6eb21b2d78044606eeb806d8cdd38640c8deea071122d5591edb88795"
    );
    assert_eq!(metadata["provider_count"], 180);
    assert_eq!(metadata["provider_model_count"], 6_131);
    assert_eq!(metadata["canonical_model_count"], 293);
    assert_eq!(metadata["test_only"], true);
    assert_eq!(metadata["runtime_pin"], false);
    assert_eq!(metadata["contains_secrets"], false);

    let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let providers = document["providers"].as_object().unwrap();
    assert_eq!(providers.len(), 180);
    assert_eq!(
        providers
            .values()
            .map(|provider| provider["models"].as_object().unwrap().len())
            .sum::<usize>(),
        6_131
    );
    assert_eq!(document["models"].as_object().unwrap().len(), 293);
    assert_no_secret_material(&fixture, &bytes);
}

#[test]
fn catalog_cache_source_has_only_the_fixed_persistent_layout() {
    let catalog = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/catalog");
    let source = fs::read_dir(catalog)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    for forbidden in [
        "models-dev-v1.current.json",
        "CatalogCacheCurrentV1",
        "generation_file(",
        ".generation",
    ] {
        assert!(
            !source.contains(forbidden),
            "forbidden catalog cache generation layout: {forbidden}"
        );
    }
    for required in [
        "models-dev-v2.json",
        "models-dev-v2.meta.json",
        "models-dev-v2.lock",
    ] {
        assert!(
            source.contains(required),
            "missing fixed cache path: {required}"
        );
    }
}
