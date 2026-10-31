#[cfg(unix)]
use std::collections::HashMap;

use clap::Parser as _;

use super::*;

const TARGET: &str = "x86_64-unknown-linux-gnu";

fn installed(tag: &str) -> Installed {
    Installed {
        tag: tag.to_owned(),
        version: if tag.is_empty() {
            "0.4.0".to_owned()
        } else {
            release_version_text(tag)
        },
        channel: release::channel_of(tag),
        target: TARGET.to_owned(),
    }
}

fn args() -> UpdateArgs {
    UpdateArgs {
        check: false,
        channel: None,
        version: None,
        force: false,
    }
}

fn asset(name: &str) -> Asset {
    Asset {
        name: name.to_owned(),
        browser_download_url: format!("https://example.invalid/{name}"),
    }
}

/// A published release of `tag` with `target`'s archive and checksum.
fn release_for(tag: &str, target: &str) -> Release {
    let archive = archive_name(target);
    Release {
        tag_name: tag.to_owned(),
        draft: false,
        prerelease: release::channel_of(tag) == Channel::Nightly,
        assets: vec![
            asset(&archive),
            asset(&format!("{archive}.sha256")),
            asset("cookie_agent-installer.sh"),
        ],
    }
}

fn release(tag: &str) -> Release {
    release_for(tag, TARGET)
}

fn candidate_of(release: &Release) -> Candidate<'_> {
    installable(release, TARGET).expect("installable release")
}

#[test]
fn a_build_from_source_needs_a_channel_or_a_tag() {
    let source = installed("");
    let error = wanted(&source, &args()).unwrap_err().to_string();
    assert!(error.contains("was built from source"), "{error}");
    assert!(error.contains("cookie update --channel nightly"), "{error}");
    let to_nightly = UpdateArgs {
        channel: Some(ChannelArg::Nightly),
        ..args()
    };
    assert_eq!(
        wanted(&source, &to_nightly).unwrap(),
        Wanted::Channel(Channel::Nightly)
    );
    let pinned = UpdateArgs {
        version: Some("0.3.0".to_owned()),
        ..args()
    };
    assert_eq!(
        wanted(&source, &pinned).unwrap(),
        Wanted::Tag("v0.3.0".to_owned())
    );
}

#[test]
fn a_release_follows_its_own_channel_unless_told_otherwise() {
    assert_eq!(
        wanted(&installed("v0.4.0-alpha.100"), &args()).unwrap(),
        Wanted::Channel(Channel::Nightly)
    );
    assert_eq!(
        wanted(&installed("v0.3.0"), &args()).unwrap(),
        Wanted::Channel(Channel::Stable)
    );
    let to_stable = UpdateArgs {
        channel: Some(ChannelArg::Stable),
        ..args()
    };
    assert_eq!(
        wanted(&installed("v0.4.0-alpha.100"), &to_stable).unwrap(),
        Wanted::Channel(Channel::Stable)
    );
}

#[test]
fn a_tag_that_is_no_release_tag_is_refused() {
    let bad = UpdateArgs {
        version: Some("latest".to_owned()),
        ..args()
    };
    let error = wanted(&installed("v0.3.0"), &bad).unwrap_err().to_string();
    assert!(error.contains("is not a release tag"), "{error}");
}

#[test]
fn nightlies_are_ordered_by_stamp_across_a_stable_release() {
    // Built after v0.4.0, but semver sorts v0.4.0-alpha.300 below it and
    // below nothing else here.
    let releases = vec![
        release("v0.4.0-alpha.300"),
        release("v0.4.0"),
        release("v0.5.0-alpha.200"),
        release("v0.4.0-alpha.100"),
    ];
    let newest = newest_nightly(&releases, TARGET).unwrap();
    assert_eq!(newest.tag, "v0.4.0-alpha.300");
}

#[test]
fn a_nightly_without_this_targets_archive_is_passed_over() {
    let mut uploading = release("v0.4.0-alpha.300");
    uploading.assets.clear();
    let mut draft = release("v0.4.0-alpha.400");
    draft.draft = true;
    let releases = vec![
        draft,
        uploading,
        release_for("v0.4.0-alpha.250", "aarch64-apple-darwin"),
        release("v0.4.0-beta.9"),
        release("v0.4.0-alpha.200"),
    ];
    assert_eq!(
        newest_nightly(&releases, TARGET).unwrap().tag,
        "v0.4.0-alpha.200"
    );
    assert!(newest_nightly(&releases[..3], TARGET).is_none());
}

#[test]
fn a_release_still_uploading_differs_from_one_without_this_target() {
    let mut uploading = release("v0.5.0");
    uploading.assets = vec![asset("source.tar.gz")];
    let error = candidate(&Found::One(uploading), TARGET)
        .unwrap_err()
        .to_string();
    assert!(error.contains("still being uploaded"), "{error}");
    let other = release_for("v0.5.0", "aarch64-apple-darwin");
    let error = candidate(&Found::One(other), TARGET)
        .unwrap_err()
        .to_string();
    assert!(error.contains("has no archive for"), "{error}");
}

#[test]
fn windows_targets_take_the_zip_archive() {
    assert_eq!(
        archive_name("x86_64-pc-windows-msvc"),
        "cookie_agent-x86_64-pc-windows-msvc.zip"
    );
    assert_eq!(
        archive_name("aarch64-apple-darwin"),
        "cookie_agent-aarch64-apple-darwin.tar.xz"
    );
}

#[test]
fn a_stable_updates_only_to_a_newer_stable() {
    let stable = Wanted::Channel(Channel::Stable);
    let current = installed("v0.4.0");
    let newer = release("v0.4.1");
    let same = release("v0.4.0");
    let older = release("v0.3.0");
    assert!(is_newer(&current, &stable, &candidate_of(&newer)));
    assert!(!is_newer(&current, &stable, &candidate_of(&same)));
    assert!(!is_newer(&current, &stable, &candidate_of(&older)));
}

#[test]
fn a_nightly_updates_only_to_a_later_nightly() {
    let nightly = Wanted::Channel(Channel::Nightly);
    let current = installed("v0.4.0-alpha.200");
    let later = release("v0.4.0-alpha.300");
    let earlier = release("v0.5.0-alpha.100");
    assert!(is_newer(&current, &nightly, &candidate_of(&later)));
    assert!(!is_newer(&current, &nightly, &candidate_of(&earlier)));
}

#[test]
fn a_switch_or_a_tag_installs_anything_but_the_installed_tag() {
    let current = installed("v0.4.0-alpha.200");
    let stable = release("v0.3.0");
    assert!(is_newer(
        &current,
        &Wanted::Channel(Channel::Stable),
        &candidate_of(&stable)
    ));
    assert!(is_newer(
        &current,
        &Wanted::Tag("v0.3.0".to_owned()),
        &candidate_of(&stable)
    ));
    let same = release("v0.4.0-alpha.200");
    assert!(!is_newer(
        &current,
        &Wanted::Tag("v0.4.0-alpha.200".to_owned()),
        &candidate_of(&same)
    ));
    assert!(is_newer(
        &installed(""),
        &Wanted::Channel(Channel::Nightly),
        &candidate_of(&same)
    ));
}

#[test]
fn only_an_older_core_version_is_a_downgrade() {
    let nightly = installed("v0.4.0-alpha.200");
    let parse = |tag| ReleaseVersion::parse(tag).unwrap();
    assert!(is_downgrade(&nightly, &parse("v0.3.0")));
    assert!(!is_downgrade(&nightly, &parse("v0.4.0")));
    assert!(!is_downgrade(&nightly, &parse("v0.4.0-alpha.100")));
    assert!(!is_downgrade(&installed(""), &parse("v0.4.0")));
}

#[test]
fn the_checksum_file_must_match_the_download() {
    let digest = "ab".repeat(32);
    let file = format!("{}  *cookie_agent-x.tar.xz\n", digest.to_uppercase());
    verify_checksum(&file, &digest, "cookie_agent-x.tar.xz").unwrap();
    let error = verify_checksum(&file, &"cd".repeat(32), "cookie_agent-x.tar.xz")
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match"), "{error}");
    let error = verify_checksum("not a digest", &digest, "cookie_agent-x.tar.xz")
        .unwrap_err()
        .to_string();
    assert!(error.contains("holds no SHA-256 digest"), "{error}");
}

#[test]
fn the_receipt_follows_only_its_own_install() {
    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join("cargo");
    fs::create_dir_all(prefix.join("bin")).unwrap();
    let prefix = fs::canonicalize(&prefix).unwrap();
    let receipt = home.path().join("cookie_agent-receipt.json");
    let original = serde_json::json!({
        "install_prefix": prefix.to_string_lossy(),
        "version": "0.4.0-alpha.100",
        "binaries": ["cookie"],
    });
    fs::write(&receipt, original.to_string()).unwrap();

    let elsewhere = home.path().join("other/cookie");
    assert!(!update_receipt(&receipt, &elsewhere, "0.4.0-alpha.200").unwrap());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&receipt).unwrap()).unwrap()
            ["version"],
        "0.4.0-alpha.100"
    );

    let binary = prefix.join("bin/cookie");
    assert!(update_receipt(&receipt, &binary, "0.4.0-alpha.200").unwrap());
    let updated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt).unwrap()).unwrap();
    assert_eq!(updated["version"], "0.4.0-alpha.200");
    assert_eq!(updated["binaries"], serde_json::json!(["cookie"]));

    assert!(!update_receipt(&home.path().join("missing.json"), &binary, "x").unwrap());
}

#[test]
fn the_daemon_hint_appears_only_for_another_version() {
    assert_eq!(daemon_hint("0.4.0-alpha.200", "0.4.0-alpha.200"), None);
    let hint = daemon_hint("0.4.0-alpha.100", "0.4.0-alpha.200").unwrap();
    assert!(hint.contains("still runs 0.4.0-alpha.100"), "{hint}");
    assert!(hint.contains("cookie daemon restart"), "{hint}");
}

#[test]
fn update_and_upgrade_parse_the_same_arguments() {
    for name in ["update", "upgrade"] {
        let cli =
            crate::cli::Cli::try_parse_from(["cookie", name, "--channel", "nightly", "--check"])
                .unwrap();
        assert_eq!(
            cli.command,
            Some(crate::cli::Command::Update(UpdateArgs {
                check: true,
                channel: Some(ChannelArg::Nightly),
                version: None,
                force: false,
            }))
        );
    }
    let cli =
        crate::cli::Cli::try_parse_from(["cookie", "update", "--version", "v0.3.0", "--force"])
            .unwrap();
    assert_eq!(
        cli.command,
        Some(crate::cli::Command::Update(UpdateArgs {
            check: false,
            channel: None,
            version: Some("v0.3.0".to_owned()),
            force: true,
        }))
    );
    assert!(
        crate::cli::Cli::try_parse_from([
            "cookie",
            "update",
            "--version",
            "v0.3.0",
            "--channel",
            "stable"
        ])
        .is_err()
    );
}

/// Serves the routes `routes` builds from the server's base URL over plain
/// HTTP/1.1, one request per connection; any other path is a 404. Returns
/// the base URL.
#[cfg(unix)]
async fn serve(routes: impl FnOnce(&str) -> HashMap<String, Vec<u8>>) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = std::sync::Arc::new(routes(&base));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = match routes.get(path) {
                    Some(body) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"{}".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.ok();
                stream.write_all(&body).await.ok();
            });
        }
    });
    base
}

/// A `.tar.xz` release archive holding `binary` as
/// `cookie_agent-<target>/cookie`, as cargo-dist packs it.
#[cfg(unix)]
fn tar_xz(binary: &[u8]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, format!("cookie_agent-{TARGET}/cookie"), binary)
        .unwrap();
    let tar = tar.into_inner().unwrap();
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut io::Cursor::new(tar), &mut xz).unwrap();
    xz
}

/// A shell script standing in for a cookie binary: prints `version_line`
/// and exits with `exit`.
#[cfg(unix)]
fn script(version_line: &str, exit: u8) -> Vec<u8> {
    format!("#!/bin/sh\necho '{version_line}'\nexit {exit}\n").into_bytes()
}

/// A release server whose latest stable is `v0.5.0` with `archive`,
/// published with the digest `checksum`, the archive's own when `None`.
#[cfg(unix)]
async fn stable_server(archive: Vec<u8>, checksum: Option<String>) -> String {
    let name = archive_name(TARGET);
    let digest = checksum.unwrap_or_else(|| hex(&Sha256::digest(&archive)));
    serve(move |base| {
        let release = serde_json::json!({
            "tag_name": "v0.5.0",
            "prerelease": false,
            "assets": [
                {"name": name, "browser_download_url": format!("{base}/download/{name}")},
                {"name": format!("{name}.sha256"),
                 "browser_download_url": format!("{base}/download/{name}.sha256")},
            ],
        });
        HashMap::from([
            (
                "/releases/latest".to_owned(),
                release.to_string().into_bytes(),
            ),
            (
                format!("/download/{name}.sha256"),
                format!("{digest}  {name}\n").into_bytes(),
            ),
            (format!("/download/{name}"), archive),
        ])
    })
    .await
}

/// Updates an installed stable `0.4.0` at `executable` from `base`, with
/// the receipt `receipt-for-test.json` beside it.
#[cfg(unix)]
async fn update_from(base: &str, executable: &Path) -> anyhow::Result<Option<String>> {
    let releases = Releases::new(base, None).unwrap();
    let receipt = executable.with_file_name("receipt-for-test.json");
    update(
        &releases,
        &installed("v0.4.0"),
        &Wanted::Channel(Channel::Stable),
        &args(),
        Some(InstallAt {
            executable,
            receipt: Some(receipt),
        }),
    )
    .await
}

/// An installed `cookie` 0.4.0 in `directory`.
#[cfg(unix)]
fn installed_binary(directory: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = directory.join("cookie");
    fs::write(&path, script("cookie 0.4.0 (stable)", 0)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    path
}

/// Files in `directory` other than the binary: the scratch directory goes
/// on every path.
#[cfg(unix)]
fn leftovers(directory: &Path) -> Vec<String> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "cookie")
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn a_newer_stable_replaces_the_binary_with_its_mode_kept() {
    use std::os::unix::fs::PermissionsExt as _;
    let new = script("cookie 0.5.0 (stable, 1a2b3c4d5)", 0);
    let base = stable_server(tar_xz(&new), None).await;
    let directory = tempfile::tempdir().unwrap();
    let executable = installed_binary(directory.path());
    let receipt = directory.path().join("receipt-for-test.json");
    let prefix = fs::canonicalize(directory.path()).unwrap();
    fs::write(
        &receipt,
        serde_json::json!({"install_prefix": prefix, "version": "0.4.0"}).to_string(),
    )
    .unwrap();

    let updated = update_from(&base, &executable).await.unwrap();

    assert_eq!(updated.as_deref(), Some("0.5.0"));
    assert_eq!(fs::read(&executable).unwrap(), new);
    let mode = fs::metadata(&executable).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o751);
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt).unwrap()).unwrap();
    assert_eq!(receipt["version"], "0.5.0");
    fs::remove_file(directory.path().join("receipt-for-test.json")).unwrap();
    assert!(leftovers(directory.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_checksum_mismatch_replaces_nothing() {
    let new = script("cookie 0.5.0 (stable)", 0);
    let base = stable_server(tar_xz(&new), Some("00".repeat(32))).await;
    let directory = tempfile::tempdir().unwrap();
    let executable = installed_binary(directory.path());
    let before = fs::read(&executable).unwrap();

    let error = update_from(&base, &executable)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("does not match"), "{error}");
    assert_eq!(fs::read(&executable).unwrap(), before);
    assert!(leftovers(directory.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_binary_that_does_not_run_replaces_nothing() {
    let base = stable_server(tar_xz(&script("cannot execute binary file", 126)), None).await;
    let directory = tempfile::tempdir().unwrap();
    let executable = installed_binary(directory.path());
    let before = fs::read(&executable).unwrap();

    let error = update_from(&base, &executable)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("does not run here"), "{error}");
    assert_eq!(fs::read(&executable).unwrap(), before);
    assert!(leftovers(directory.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_binary_of_another_version_replaces_nothing() {
    let base = stable_server(tar_xz(&script("cookie 0.3.9 (stable)", 0)), None).await;
    let directory = tempfile::tempdir().unwrap();
    let executable = installed_binary(directory.path());

    let error = update_from(&base, &executable)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("does not run here"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn the_installed_stable_is_left_alone() {
    let base = serve(|base| {
        let name = archive_name(TARGET);
        let release = serde_json::json!({
            "tag_name": "v0.4.0",
            "assets": [
                {"name": name, "browser_download_url": format!("{base}/download/{name}")},
                {"name": format!("{name}.sha256"),
                 "browser_download_url": format!("{base}/download/{name}.sha256")},
            ],
        });
        HashMap::from([(
            "/releases/latest".to_owned(),
            release.to_string().into_bytes(),
        )])
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let executable = installed_binary(directory.path());
    let before = fs::read(&executable).unwrap();

    assert_eq!(update_from(&base, &executable).await.unwrap(), None);
    assert_eq!(fs::read(&executable).unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn an_unknown_tag_is_reported() {
    let base = serve(|_| HashMap::new()).await;
    let releases = Releases::new(&base, None).unwrap();
    let error = update(
        &releases,
        &installed("v0.4.0"),
        &Wanted::Tag("v9.9.9".to_owned()),
        &args(),
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("no release is tagged v9.9.9"), "{error}");
}

#[test]
fn the_smoke_test_matches_the_version_as_a_whole() {
    for printed in [
        "cookie 1.2.3\n",
        "cookie 1.2.3 (stable, 0123abcde)\n",
        "cookie 1.2.3-alpha.1793047587 (nightly, 0123abcde)\n",
        "cookie 1.2.3+build.7\n",
    ] {
        assert!(prints_version(printed, "1.2.3"), "{printed}");
    }
    for printed in [
        "cookie 1.2.30 (stable)\n",
        "cookie 11.2.3\n",
        "cookie 1.2.3.4\n",
        "cookie 1.2\n",
        "cookie-agent 1.2.3\n",
        "other 1.2.3\n",
        "",
    ] {
        assert!(!prints_version(printed, "1.2.3"), "{printed}");
    }
}
