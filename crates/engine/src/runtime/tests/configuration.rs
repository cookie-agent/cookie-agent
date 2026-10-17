//! Reloading the configuration of a running engine.

use cookie_agent_protocol::RuntimeChangeReason;

use crate::{EngineError, RestartRequiredPart};

use super::support::*;

const PRIMARY: &str = "---\ndescription: Bedrock test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0\", variant: base }]\npermissions: {}\n---\nUse Bedrock.\n";
const REVIEWER: &str = "---\ndescription: Reviewer\nmode: subagent\nenabled: true\nmodels: [{ model: \"amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0\", variant: base }]\npermissions: {}\n---\nReview.\n";

struct Reloadable {
    _directory: PanicResistantTempDir,
    root: std::path::PathBuf,
    engine: crate::Engine,
}

fn reloadable() -> Reloadable {
    let directory = private_tempdir();
    let workspace = directory.path().join("workspace");
    let config = empty_provider_workspace(&workspace);
    let (engine, _) = open_workspace_engine(
        &workspace,
        &directory.path().join("data"),
        &directory.path().join("provider-store"),
        bedrock_catalog(),
        config,
    );
    Reloadable {
        root: workspace.join(".cookie-agent"),
        _directory: directory,
        engine,
    }
}

impl Reloadable {
    fn write(&self, path: &str, text: &str) {
        write_private_test_file(&self.root.join(path), text);
    }

    fn snapshot(&self) -> cookie_agent_protocol::RuntimeSnapshotV1 {
        self.engine.runtime_snapshot().expect("snapshot").snapshot
    }
}

#[tokio::test]
async fn a_reload_applies_settings_and_agents_and_publishes_once() {
    let fixture = reloadable();
    let before = fixture.snapshot();
    assert!(fixture.engine.configuration().runtime.loop_warning.enabled);
    let mut changes = fixture.engine.subscribe_runtime_changes();

    fixture.write("config.toml", "[loop_warning]\nenabled = false\n");
    fixture.write("agents/reviewer.md", REVIEWER);
    let status = fixture.engine.reload_configuration().await.expect("reload");

    assert!(!fixture.engine.configuration().runtime.loop_warning.enabled);
    assert_eq!(status.error, None);
    assert!(status.restart_required.is_empty());
    let after = fixture.snapshot();
    assert_eq!(after.config_revision, status.applied_revision);
    assert_ne!(after.config_revision, before.config_revision);
    assert_ne!(after.runtime_revision, before.runtime_revision);
    assert!(
        after
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "reviewer")
    );
    let notification = changes.try_recv().expect("one runtime change");
    assert_eq!(
        notification.reasons,
        [
            RuntimeChangeReason::ConfigReloaded,
            RuntimeChangeReason::AgentReloaded
        ]
    );
    assert!(changes.try_recv().is_err());

    // Settings alone reload without an agent change.
    fixture.write("config.toml", "");
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        changes.try_recv().expect("runtime change").reasons,
        [RuntimeChangeReason::ConfigReloaded]
    );
}

#[tokio::test]
async fn an_invalid_configuration_keeps_the_applied_one_and_reports_why() {
    let fixture = reloadable();
    let before = fixture.snapshot();
    let applied = fixture.engine.configuration().revision.clone();

    fixture.write("config.toml", "[unknown]\n");
    assert!(matches!(
        fixture.engine.reload_configuration().await,
        Err(EngineError::Config(_))
    ));
    let status = fixture.engine.configuration_status();
    assert_eq!(status.applied_revision, applied);
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown")),
        "{status:?}"
    );
    assert_eq!(fixture.snapshot(), before);

    // A broken delegation is caught across documents.
    fixture.write("config.toml", "");
    fixture.write(
        "agents/primary.md",
        &PRIMARY.replace(
            "permissions: {}",
            "permissions:\n  delegate:\n    ghost: allow",
        ),
    );
    assert!(fixture.engine.reload_configuration().await.is_err());

    // The next good reload clears the error.
    fixture.write("agents/primary.md", PRIMARY);
    let status = fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(status.error, None);
}

#[tokio::test]
async fn parts_that_need_a_restart_are_reported_until_they_match_again() {
    let fixture = reloadable();
    fixture.write(
        "config.toml",
        "[server]\nport = 7500\n\n[runtimes.labpc]\ncommand = [\"ssh\", \"labpc\", \"cookie\", \"runtime\", \"serve\", \"--stdio\"]\n",
    );
    let status = fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        status.restart_required,
        [RestartRequiredPart::Server, RestartRequiredPart::Runtimes]
    );
    // The daemon still runs the templates it started with.
    assert!(
        !fixture
            .engine
            .runtime_templates()
            .iter()
            .any(|template| template.runtime_id == "labpc")
    );
    fixture.write("config.toml", "");
    let status = fixture.engine.reload_configuration().await.expect("reload");
    assert!(status.restart_required.is_empty());
}

#[tokio::test]
async fn mcp_servers_follow_the_reloaded_file_under_the_runtime_layer() {
    let fixture = reloadable();
    let names = |engine: &crate::Engine| {
        engine
            .list_mcp_servers()
            .into_iter()
            .filter(|server| server.runtime_id.as_str() == "global")
            .map(|server| server.name)
            .collect::<Vec<_>>()
    };
    fixture.write(
        "config.toml",
        "[mcp.servers.docs]\ncommand = \"cookie-test-missing-docs-mcp\"\nenabled = false\n",
    );
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(names(&fixture.engine), ["docs"]);

    fixture.write("config.toml", "");
    fixture.engine.reload_configuration().await.expect("reload");
    assert!(names(&fixture.engine).is_empty());
}
