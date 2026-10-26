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

    fn snapshot(&self) -> cookie_agent_protocol::ModelCatalogView {
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
    assert_eq!(
        fixture.engine.current_runtime().revisions.config_revision,
        status.applied_revision
    );
    assert_ne!(after.revision, before.revision);
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

    // A broken delegation is caught across documents: the primary cannot
    // delegate to an agent of mode `primary`, itself.
    fixture.write("config.toml", "");
    fixture.write(
        "agents/primary.md",
        &PRIMARY.replace(
            "permissions: {}",
            "permissions:\n  delegate:\n    primary: allow",
        ),
    );
    assert!(fixture.engine.reload_configuration().await.is_err());

    // A target no agent has applies, with a diagnostic naming it.
    fixture.write(
        "agents/primary.md",
        &PRIMARY.replace(
            "permissions: {}",
            "permissions:\n  delegate:\n    ghost: allow",
        ),
    );
    let status = fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(status.error, None);
    assert!(
        status
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("`ghost`, which is not known yet")),
        "{status:?}"
    );

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

/// A token the daemon created at start and wrote to the file is the one it
/// serves with, so the file holding it is no `server` change; another
/// token is.
#[tokio::test]
async fn the_token_the_daemon_wrote_at_start_needs_no_restart() {
    let fixture = reloadable();
    let written = "B".repeat(43);
    fixture.write("config.toml", &format!("[server]\ntoken = \"{written}\"\n"));
    let status = fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(status.restart_required, [RestartRequiredPart::Server]);
    fixture.engine.note_served_server_token(&written);
    assert!(
        fixture
            .engine
            .configuration_status()
            .restart_required
            .is_empty()
    );
    let other = "C".repeat(43);
    fixture.write("config.toml", &format!("[server]\ntoken = \"{other}\"\n"));
    let status = fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(status.restart_required, [RestartRequiredPart::Server]);
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

fn path(path: &str) -> cookie_agent_config::ConfigPath {
    cookie_agent_config::ConfigPath::new(path).expect("valid path")
}

#[tokio::test]
async fn a_change_set_is_written_validated_and_applied_together() {
    use crate::ConfigChange;
    let fixture = reloadable();
    let read = fixture
        .engine
        .configuration_documents()
        .await
        .expect("read");
    let primary = read
        .documents
        .iter()
        .find(|document| document.path.as_str() == "agents/primary.md")
        .expect("primary")
        .clone();

    // The delegator and its new subagent go in together.
    let revision = fixture
        .engine
        .apply_config_changes(
            read.revision.clone(),
            vec![
                ConfigChange::Put {
                    path: path("agents/reviewer.md"),
                    text: REVIEWER.into(),
                    expected_fingerprint: None,
                },
                ConfigChange::Put {
                    path: path("agents/primary.md"),
                    text: PRIMARY.replace(
                        "permissions: {}",
                        "permissions:\n  delegate:\n    reviewer: allow",
                    ),
                    expected_fingerprint: Some(primary.fingerprint.clone()),
                },
                ConfigChange::Set {
                    key: vec!["loop_warning".into(), "enabled".into()],
                    value: serde_json::json!(false),
                },
            ],
        )
        .await
        .expect("apply");
    assert_eq!(fixture.engine.configuration().revision, revision);
    assert!(!fixture.engine.configuration().runtime.loop_warning.enabled);
    assert!(fixture.root.join("agents/reviewer.md").exists());
    assert!(
        std::fs::read_to_string(fixture.root.join("config.toml"))
            .unwrap()
            .contains("enabled = false")
    );
    assert!(
        fixture
            .snapshot()
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "reviewer")
    );
    let reread = fixture
        .engine
        .configuration_documents()
        .await
        .expect("read");
    assert_eq!(reread.revision, revision);

    // A stale revision, a create of an existing document, and a stale
    // fingerprint each change nothing.
    let delete_reviewer = |fingerprint: String| ConfigChange::Delete {
        path: path("agents/reviewer.md"),
        expected_fingerprint: fingerprint,
    };
    for (expected, change) in [
        (read.revision.clone(), delete_reviewer(String::new())),
        (
            revision.clone(),
            ConfigChange::Put {
                path: path("agents/reviewer.md"),
                text: REVIEWER.into(),
                expected_fingerprint: None,
            },
        ),
        (
            revision.clone(),
            delete_reviewer(primary.fingerprint.clone()),
        ),
    ] {
        assert!(matches!(
            fixture
                .engine
                .apply_config_changes(expected, vec![change])
                .await,
            Err(EngineError::ConfigConflict(_))
        ));
    }
    // A result that is not a valid configuration changes nothing: the
    // primary still delegates to the reviewer, which cannot be of mode
    // `primary`.
    let reviewer = reread
        .documents
        .iter()
        .find(|document| document.path.as_str() == "agents/reviewer.md")
        .unwrap()
        .fingerprint
        .clone();
    assert!(matches!(
        fixture
            .engine
            .apply_config_changes(
                revision.clone(),
                vec![ConfigChange::Put {
                    path: path("agents/reviewer.md"),
                    text: REVIEWER.replace("mode: subagent", "mode: primary"),
                    expected_fingerprint: Some(reviewer),
                }],
            )
            .await,
        Err(EngineError::Config(_))
    ));
    assert!(fixture.root.join("agents/reviewer.md").exists());
    // A set and a put of config.toml cannot go together.
    assert!(matches!(
        fixture
            .engine
            .apply_config_changes(
                revision.clone(),
                vec![
                    ConfigChange::Set {
                        key: vec!["loop_warning".into(), "enabled".into()],
                        value: serde_json::Value::Null,
                    },
                    ConfigChange::Put {
                        path: path("config.toml"),
                        text: String::new(),
                        expected_fingerprint: None,
                    },
                ]
            )
            .await,
        Err(EngineError::ConfigChangeRefused(_))
    ));
    assert_eq!(fixture.engine.configuration().revision, revision);
}

#[tokio::test]
async fn persisting_an_mcp_server_writes_and_applies_the_configuration() {
    let fixture = reloadable();
    let definition = cookie_agent_protocol::McpServerDefinition {
        command: Some("cookie-test-missing-docs-mcp".into()),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
        url: None,
        headers: Default::default(),
        oauth: None,
        enabled: false,
        timeout_ms: None,
        include_tools: None,
        exclude_tools: Vec::new(),
        scope: None,
    };
    fixture
        .engine
        .add_mcp_server("docs".into(), definition)
        .await
        .expect("add");
    let before = fixture.engine.configuration().revision.clone();
    let persisted = fixture
        .engine
        .persist_mcp_server(
            "docs".into(),
            cookie_agent_protocol::McpConfigTarget::UserFile,
        )
        .await
        .expect("persist")
        .expect("listed");
    assert_eq!(
        persisted.source,
        cookie_agent_protocol::McpConfigSource::UserFile
    );
    assert_ne!(fixture.engine.configuration().revision, before);
    assert!(
        fixture
            .engine
            .configuration()
            .user_mcp_servers
            .contains_key("docs")
    );
    assert!(
        std::fs::read_to_string(fixture.root.join("config.toml"))
            .unwrap()
            .contains("[mcp.servers.docs]")
    );

    // Persisting a removal deletes the table.
    fixture
        .engine
        .remove_mcp_server("docs".into())
        .await
        .expect("remove");
    fixture
        .engine
        .persist_mcp_server(
            "docs".into(),
            cookie_agent_protocol::McpConfigTarget::UserFile,
        )
        .await
        .expect("persist the removal");
    assert!(
        !std::fs::read_to_string(fixture.root.join("config.toml"))
            .unwrap()
            .contains("docs")
    );
    assert!(
        !fixture
            .engine
            .configuration()
            .user_mcp_servers
            .contains_key("docs")
    );
}

fn runtime_server(command: &str) -> cookie_agent_protocol::McpServerDefinition {
    cookie_agent_protocol::McpServerDefinition {
        command: Some(command.into()),
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        cwd: None,
        url: None,
        headers: std::collections::BTreeMap::new(),
        oauth: None,
        enabled: true,
        timeout_ms: None,
        include_tools: None,
        exclude_tools: Vec::new(),
        scope: Some(cookie_agent_protocol::McpServerScope::Runtime),
    }
}

/// A runtime's MCP servers are changed in its `[runtimes.<id>]` table,
/// written and applied (`docs/site/specs/runtime-management.md` section
/// 4.1).
#[tokio::test]
async fn a_runtimes_mcp_servers_are_added_edited_renamed_and_removed_in_its_table() {
    let fixture = reloadable();
    let engine = &fixture.engine;
    let labpc = cookie_agent_protocol::RuntimeId::new("labpc").unwrap();
    let servers = |engine: &crate::Engine| {
        engine
            .configuration()
            .runtimes
            .iter()
            .find(|(id, _)| id.as_str() == "labpc")
            .map(|(_, template)| template.mcp_servers.clone())
            .unwrap_or_default()
    };
    engine
        .add_runtime_mcp_server(
            labpc.clone(),
            "browser".into(),
            runtime_server("browser-mcp"),
        )
        .await
        .expect("added");
    let text = std::fs::read_to_string(fixture.root.join("config.toml")).unwrap();
    assert!(
        text.contains("[runtimes.labpc.mcp.servers.browser]"),
        "{text}"
    );
    let added = &servers(engine)["browser"];
    assert_eq!(added.command.as_deref(), Some("browser-mcp"));
    assert_eq!(
        added.scope,
        Some(cookie_agent_config::McpServerScope::Runtime)
    );
    // The table holds a dial-in runtime's settings: nothing to start.
    assert!(
        !engine
            .configuration()
            .runtimes
            .values()
            .next()
            .unwrap()
            .daemon_starts()
    );
    let again = engine
        .add_runtime_mcp_server(labpc.clone(), "browser".into(), runtime_server("other"))
        .await
        .expect_err("a second `browser`");
    assert!(again.to_string().contains("already has"), "{again}");

    engine
        .set_runtime_mcp_server_enabled(labpc.clone(), "browser".into(), false)
        .await
        .expect("disabled");
    assert!(!servers(engine)["browser"].enabled);
    let mut edited = runtime_server("browser-mcp");
    edited.exclude_tools = vec!["browser_install".into()];
    engine
        .edit_runtime_mcp_server(labpc.clone(), "browser".into(), edited)
        .await
        .expect("edited");
    assert_eq!(
        servers(engine)["browser"].exclude_tools,
        ["browser_install"]
    );

    engine
        .rename_mcp_server(Some(labpc.clone()), "browser".into(), "chrome".into())
        .await
        .expect("renamed");
    let renamed = servers(engine);
    assert!(!renamed.contains_key("browser"));
    assert_eq!(renamed["chrome"].exclude_tools, ["browser_install"]);
    engine
        .remove_runtime_mcp_server(labpc.clone(), "chrome".into())
        .await
        .expect("removed");
    assert!(servers(engine).is_empty());
    let unknown = engine
        .remove_runtime_mcp_server(labpc, "chrome".into())
        .await
        .expect_err("gone");
    assert!(unknown.to_string().contains("no MCP server"), "{unknown}");

    // `global`'s servers are the top-level ones, which have no table here.
    let global = cookie_agent_protocol::RuntimeId::new("global").unwrap();
    let refused = engine
        .add_runtime_mcp_server(global, "x".into(), runtime_server("x"))
        .await
        .expect_err("no [runtimes.global]");
    assert!(refused.to_string().contains("[mcp]"), "{refused}");
}

/// A runtime's ACP agent declarations are set, listed with the documents
/// that run them, renamed with their own document when nothing else
/// declares them, and removed (section 4.2, decision D6).
#[tokio::test]
async fn a_runtimes_acp_agents_are_set_listed_renamed_and_removed() {
    let fixture = reloadable();
    fixture.write(
        "agents/claude.md",
        "---\ndescription: Claude Code\nmode: all\nacp: {}\n---\nWork.\n",
    );
    fixture.write(
        "agents/claude-review.md",
        "---\ndescription: Reviewer\nmode: subagent\nacp:\n  agent: claude\n---\nReview.\n",
    );
    fixture.engine.reload_configuration().await.expect("reload");
    let engine = &fixture.engine;
    let labpc = cookie_agent_protocol::RuntimeId::new("labpc").unwrap();
    let claude = cookie_agent_protocol::AgentId::new("claude").unwrap();
    let definition = cookie_agent_protocol::AcpAgentDefinition {
        command: vec!["claude-acp".into()],
        env: std::collections::BTreeMap::new(),
        description: "Claude Code".into(),
        enabled: true,
    };
    let listed = engine
        .set_runtime_agent(labpc.clone(), claude.clone(), definition)
        .await
        .expect("declared");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].agent, claude);
    assert_eq!(listed[0].definition.command, ["claude-acp"]);
    assert!(!listed[0].available, "labpc is not connected");
    assert_eq!(
        listed[0].documents,
        ["agents/claude-review.md", "agents/claude.md"]
    );

    let renamed = cookie_agent_protocol::AgentId::new("claude-code").unwrap();
    let (agents, moved, stale) = engine
        .rename_runtime_agent(labpc.clone(), claude, renamed.clone())
        .await
        .expect("renamed");
    assert_eq!(agents[0].agent, renamed);
    assert_eq!(moved, ["agents/claude.md"]);
    assert_eq!(stale, ["agents/claude-review.md"]);
    assert!(fixture.root.join("agents/claude-code.md").exists());
    assert!(!fixture.root.join("agents/claude.md").exists());
    assert_eq!(agents[0].documents, ["agents/claude-code.md"]);

    let agents = engine
        .remove_runtime_agent(labpc.clone(), renamed.clone())
        .await
        .expect("removed");
    assert!(agents.is_empty());
    // Its document stays.
    assert!(fixture.root.join("agents/claude-code.md").exists());
    assert!(engine.runtime_agent_declarations(&labpc).is_empty());
}
