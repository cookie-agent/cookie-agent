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

fn sha256(path: impl AsRef<Path>) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
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

fn tree_sha256(root: &Path, excluded: &[&str]) -> (usize, String) {
    let mut files = Vec::new();
    collect_files(root, root, &mut files);
    files.retain(|path| !excluded.iter().any(|excluded| path == Path::new(excluded)));
    files.sort();

    let mut tree = Sha256::new();
    for relative in &files {
        tree.update(relative.to_string_lossy().as_bytes());
        tree.update([0]);
        tree.update(fs::read(root.join(relative)).unwrap());
        tree.update([0]);
    }
    (files.len(), format!("{:x}", tree.finalize()))
}

const WORKSPACE_MANIFESTS: &[&str] = &[
    "crates/identity/Cargo.toml",
    "crates/config/Cargo.toml",
    "crates/cookie_agent/Cargo.toml",
    "crates/engine/Cargo.toml",
    "crates/models/Cargo.toml",
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
        .filter(|path| {
            !path.starts_with("vendor")
                && !path.starts_with("target")
                && !path.starts_with("assets")
        })
        .map(|path| root.join(path))
        .filter(|path| path.is_file())
        .collect()
}

#[test]
fn published_oven_dependencies_are_exactly_pinned() {
    let manifest = fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    for pin in [
        "oven-sdk = \"=0.5.0\"",
        "oven-sdk-anthropic = \"=0.6.0\"",
        "oven-sdk-openai = \"=0.5.0\"",
        "oven-sdk-google = \"=0.5.0\"",
        "oven-sdk-google-vertex = \"=0.5.0\"",
        "oven-sdk-bedrock = \"=0.4.0\"",
        "oven-sdk-azure = \"=0.4.0\"",
        "oven-sdk-cohere = \"=0.3.0\"",
        "oven-sdk-open-responses = \"=0.3.0\"",
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
    assert!(manifest.contains(&["oven-sdk-open", "-responses.workspace = true"].concat()));
}

#[test]
fn syntect_patch_is_exactly_pinned_and_declared() {
    let manifest = fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("syntect = { version = \"=5.3.0\""));
    assert!(manifest.contains("syntect = { path = \"vendor/syntect\" }"));
    let root = manifest.parse::<toml::Value>().unwrap();
    let syntect = root["workspace"]["dependencies"]["syntect"]
        .as_table()
        .unwrap();
    assert_eq!(syntect["version"].as_str(), Some("=5.3.0"));
    assert_eq!(syntect["default-features"].as_bool(), Some(false));
    assert_eq!(
        syntect["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|feature| feature.as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["default-syntaxes", "default-themes", "regex-fancy"])
    );

    let vendor = workspace().join("vendor/syntect");
    let vendored_manifest = fs::read_to_string(vendor.join("Cargo.toml")).unwrap();
    for declaration in [
        "[dependencies.bincode]",
        "version = \"1.3.3\"",
        "optional = true",
    ] {
        assert!(
            vendored_manifest.contains(declaration),
            "missing bincode declaration: {declaration}"
        );
    }

    let lock = fs::read_to_string(workspace().join("Cargo.lock")).unwrap();
    assert!(lock.contains("name = \"bincode\"\n"));
    assert!(!lock.contains("name = \"syntect-bincode-compat\"\n"));
    assert!(!lock.contains("name = \"bincode_reloaded\"\n"));
}

#[test]
fn vendored_syntect_matches_declared_upstream_delta() {
    let vendor = workspace().join("vendor/syntect");
    let provenance = fs::read_to_string(vendor.join("README.cookie-agent.md")).unwrap();
    for required in [
        "https://github.com/trishume/syntect",
        "v5.3.0",
        "e4670846ecf16d8832db6c43d531bec466214e27",
        "656b45c05d95a5704399aeef6bd0ddec7b2b3531b7c9e900abbf7c4d2190c925",
        "not regenerated.",
    ] {
        assert!(
            provenance.contains(required),
            "missing provenance: {required}"
        );
    }

    assert_eq!(
        sha256(vendor.join("Cargo.toml")),
        "f2e94171e18e8dc4bd510f0481248bd88ea651b8f925e31520f321957a771ec9"
    );
    assert!(!vendor.join("Cargo.lock").exists());
    assert_eq!(
        sha256(vendor.join("src/dumps.rs")),
        "237719802be45db966a6e2e5de2f58baa970ac84f83fb375dbdd90592e704e91"
    );
    assert_eq!(
        sha256(vendor.join("tests/public_api.rs")),
        "8e2454ad58226b2ecf01fd1a73f6ad2c04e98f04022379ed5e5f5f89828676d9"
    );
    assert_eq!(
        sha256(vendor.join("tests/snapshots/public-api.txt")),
        "7a8b4cb34bd3bb01c507c9990faa93d902741a2f08050a60a2f76bacdcd545bd"
    );
    for (asset, expected) in [
        (
            "assets/default.themedump",
            "8b57a2118224993360b6fc5fc2fa2e9872a827f00f9c57d43da08fa42c892399",
        ),
        (
            "assets/default_metadata.packdump",
            "b1df0402dfdb84b9826b206bffafb35553c46530afcbb3c929147760056766f3",
        ),
        (
            "assets/default_newlines.packdump",
            "d740b20c12e40b678b9f1012401e1969aaa5cd55f1ab329ffeb94d746b06a5c0",
        ),
        (
            "assets/default_nonewlines.packdump",
            "b61623ff9b5c36e60666d637076697ad8234116b2d53ad2ee9e3908df1c2461d",
        ),
    ] {
        assert_eq!(
            sha256(vendor.join(asset)),
            expected,
            "changed asset: {asset}"
        );
    }

    assert_eq!(
        tree_sha256(
            &vendor,
            &["Cargo.toml", "Cargo.lock", "README.cookie-agent.md"]
        ),
        (
            57,
            "15fd18cb2fb1441f3773b6ed900f656a89dc4a72c4ea6248d6a702574eac4e63".to_owned()
        )
    );
}

#[test]
fn every_internal_path_dependency_has_its_exact_package_version() {
    for manifest in PHASE1_MANIFESTS
        .iter()
        .copied()
        .chain(["vendor/syntect/Cargo.toml"])
    {
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
fn workspace_metadata_marks_every_cookie_crate_nonpublishable_and_uses_vendored_syntect() {
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
        assert_eq!(package["publish"].as_array().map(Vec::len), Some(0));
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
    assert!(syntect_package["source"].is_null());
    assert!(
        syntect_package["manifest_path"]
            .as_str()
            .unwrap()
            .ends_with("/vendor/syntect/Cargo.toml")
    );
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
    for required in [
        "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683",
        "dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4",
        "cargo install --locked cargo-audit --version '=0.22.2'",
        "cargo install --locked cargo-deny --version '=0.20.2'",
        "cargo build --release --locked --workspace --all-targets",
        "cargo audit --file Cargo.lock --deny yanked",
        "cargo deny --locked check advisories licenses sources",
        "cargo test --locked -p cookie_agent_models --test release_integrity",
        "release_binary_contains_no_secret_material -- --ignored --exact",
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
        ["--manifest", "-path vendor/"].concat(),
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
    assert!(manifest.contains("default-features = false"));
    assert!(manifest.contains("features = [\"json\", \"stream\", \"rustls-tls\"]"));
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
