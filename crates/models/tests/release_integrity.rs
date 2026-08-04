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
const SHIM_MANIFEST: &str = "vendor/bincode-compat/Cargo.toml";
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
        "oven-sdk = \"=0.4.0\"",
        "oven-sdk-anthropic = \"=0.5.0\"",
        "oven-sdk-openai = \"=0.4.0\"",
        "oven-sdk-google = \"=0.4.0\"",
        "oven-sdk-google-vertex = \"=0.4.0\"",
        "oven-sdk-bedrock = \"=0.3.0\"",
        "oven-sdk-azure = \"=0.3.0\"",
        "oven-sdk-cohere = \"=0.2.0\"",
        "oven-sdk-open-responses = \"=0.2.0\"",
    ] {
        assert!(manifest.contains(pin), "missing exact pin: {pin}");
    }
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
        "version = \"=0.1.0\"",
        "optional = true",
        "package = \"syntect-bincode-compat\"",
        "path = \"../bincode-compat\"",
    ] {
        assert!(
            vendored_manifest.contains(declaration),
            "missing vendored codec declaration: {declaration}"
        );
    }

    let shim = workspace().join("vendor/bincode-compat");
    let shim_manifest = fs::read_to_string(shim.join("Cargo.toml")).unwrap();
    for declaration in [
        "name = \"syntect-bincode-compat\"",
        "name = \"bincode\"",
        "version = \"=3.1.15\"",
        "path = \"../bincode-reloaded\"",
        "default-features = false",
        "features = [\"std\", \"serde\"]",
    ] {
        assert!(
            shim_manifest.contains(declaration),
            "missing shim declaration: {declaration}"
        );
    }
    let shim_source = fs::read_to_string(shim.join("src/lib.rs")).unwrap();
    for compatibility in [
        "pub type Result<T> = std::result::Result<T, Error>",
        "pub type Error = Box<ErrorKind>",
        "config::legacy()",
        "impl From<EncodeError> for Error",
        "impl From<DecodeError> for Error",
        "impl From<io::Error> for Error",
        "impl serde::ser::Error for Error",
        "impl serde::de::Error for Error",
    ] {
        assert!(
            shim_source.contains(compatibility),
            "missing shim compatibility surface: {compatibility}"
        );
    }

    let lock = fs::read_to_string(workspace().join("Cargo.lock")).unwrap();
    assert!(!lock.contains("name = \"bincode\"\n"));
    assert!(lock.contains("name = \"syntect-bincode-compat\"\n"));
    assert!(lock.contains("name = \"bincode_reloaded\"\n"));
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
        "abcbbb84b2d2e65b016cc18094ff7646f0dd84175d9fb41b674bf9df9e3ecc11"
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

    let reloaded = workspace().join("vendor/bincode-reloaded");
    assert_eq!(
        sha256(reloaded.join("Cargo.toml")),
        "cece534ba1e7cd8edae14dcf3fc55afc76751e575fcadb6de1ffd87ac8296e76"
    );
    assert!(!reloaded.join("Cargo.lock").exists());
    assert_eq!(
        tree_sha256(&reloaded, &["Cargo.toml", "README.cookie-agent.md"]),
        (
            73,
            "a492cb0139a4fc0864da8cb93a94d545653086da4f6e9c6fe40d47a20d1e6bf6".to_owned()
        )
    );

    let shim = workspace().join("vendor/bincode-compat");
    assert!(!shim.join("Cargo.lock").exists());
    let shim_provenance = fs::read_to_string(shim.join("README.md")).unwrap();
    for required in [
        "b1f45e9417d87227c7a56d22e471c6206462cba514c7590c09aff4cf6d1ddcad",
        "2e4ac690d35463a65215a28cbc1a0de736a2ed299113874f1a8cdf5d5adc231e",
        "The local shim is MIT licensed",
        "No package named `bincode` is introduced into Cargo.lock.",
    ] {
        assert!(
            shim_provenance.contains(required),
            "missing shim provenance: {required}"
        );
    }
    assert_eq!(
        tree_sha256(&shim, &["README.md"]),
        (
            3,
            "3ee8e7fff21fc93b36eda6debba78c7c16ade35a5121c5dd1d79f7aee1de44fc".to_owned()
        )
    );
}

#[test]
fn every_internal_path_dependency_has_its_exact_package_version() {
    for manifest in PHASE1_MANIFESTS
        .iter()
        .copied()
        .chain([SHIM_MANIFEST, "vendor/syntect/Cargo.toml"])
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
    assert_eq!(workspace_packages.len(), WORKSPACE_MANIFESTS.len() + 1);
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
    assert!(workspace_packages.iter().any(|package| {
        package["name"] == "syntect-bincode-compat"
            && package["manifest_path"]
                .as_str()
                .unwrap()
                .ends_with("/vendor/bincode-compat/Cargo.toml")
    }));

    for (name, expected_manifest) in [
        ("syntect", "/vendor/syntect/Cargo.toml"),
        (
            "syntect-bincode-compat",
            "/vendor/bincode-compat/Cargo.toml",
        ),
        ("bincode_reloaded", "/vendor/bincode-reloaded/Cargo.toml"),
    ] {
        let package = packages
            .iter()
            .find(|package| package["name"] == name)
            .unwrap();
        assert!(
            package["source"].is_null(),
            "{name} resolved from a registry"
        );
        assert!(
            package["manifest_path"]
                .as_str()
                .unwrap()
                .ends_with(expected_manifest),
            "{name} resolved from the wrong path"
        );
    }

    let syntect_package = packages
        .iter()
        .find(|package| package["name"] == "syntect")
        .unwrap();
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
fn model_package_has_no_registry_discovery_network_fetch_or_unapproved_adapters() {
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let source = [
        "lib.rs",
        "schema.rs",
        "catalog.rs",
        "credentials.rs",
        "manager.rs",
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
        "reqwest::get",
    ] {
        assert!(
            !source.contains(forbidden),
            "forbidden model-package behavior: {forbidden}"
        );
    }
}
