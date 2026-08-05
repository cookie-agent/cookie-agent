use std::{path::PathBuf, process::Command};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/update_models_dev.py")
}

#[test]
fn updater_self_test_proves_offline_dependency_failure_runs_no_command() {
    let output = Command::new("python3")
        .arg(script())
        .arg("--self-test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("offline mode executes no install"));
}

#[test]
fn offline_check_requires_an_explicit_prepared_source_and_never_clones() {
    let output = Command::new("python3")
        .arg(script())
        .arg("--check")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--check requires --source"));
    assert!(stderr.contains("offline mode never clones"));
}

#[test]
fn updater_names_the_artifact_as_bootstrap_not_a_runtime_revision_pin() {
    let source = std::fs::read_to_string(script()).unwrap();
    assert!(source.contains("BOOTSTRAP_OUTPUT"));
    assert!(source.contains("LICENSE_OUTPUT"));
    assert!(source.contains("PROVENANCE_OUTPUT"));
    assert!(source.contains("INTEGRITY_CONSTANTS"));
    assert!(source.contains("runtime network selection remains unpinned"));
    assert!(!source.contains("models-dev-claims-synthetic"));
    assert!(!source.contains("models-dev-live-audit-2026-08-05"));
    assert!(!source.contains("required runtime revision"));
}
