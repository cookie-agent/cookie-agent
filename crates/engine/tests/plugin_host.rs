use std::{collections::BTreeMap, fs, sync::Arc, time::Duration};

use cookie_agent_config::load_from_roots;
use cookie_agent_engine::{Engine, EngineOptions, PluginState};
use cookie_agent_identity::CatalogRevision;
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use jiff::Timestamp;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");
const MCP_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py");

fn tools(name: &str) -> String {
    format!(
        r#"[{{"name":"{name}","description":"Fixture tool","parameters":{{"type":"object","properties":{{}}}},"permission_name":"{name}","primary_resource_param":null}}]"#
    )
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn python_command() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn plugin_table(name: &str, env: &[(&str, String)], extra: &str) -> String {
    let environment = env
        .iter()
        .map(|(key, value)| format!("{key} = {}", toml_string(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[plugins.{name}]\ncommand = {}\nargs = [{}]\nenv = {{ {environment} }}\n{extra}\n",
        toml_string(python_command()),
        toml_string(FIXTURE)
    )
}

fn mcp_table(name: &str, lazy: bool) -> String {
    format!(
        "[mcp.servers.{name}]\ncommand = {}\nargs = [{}]\nlazy = {lazy}\n",
        toml_string(python_command()),
        toml_string(MCP_FIXTURE)
    )
}

fn private_tempdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("workspace");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private workspace");
    }
    #[cfg(windows)]
    {
        fs::remove_dir(directory.path()).expect("remove ordinary temp directory");
        cookie_agent_models::secure_store::SecureDirectory::open(directory.path())
            .expect("private workspace");
    }
    directory
}

fn create_private_test_dir(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir(path).expect("private test directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("private test directory");
    }
    #[cfg(windows)]
    cookie_agent_models::secure_store::SecureDirectory::open(path).expect("private test directory");
}

fn write_private_test_file(path: &std::path::Path, contents: &[u8]) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, contents).expect("private test file");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private test file");
    }
    #[cfg(windows)]
    {
        use std::io::Write as _;

        let mut file = cookie_agent_models::secure_store::create_windows_private_file(path)
            .expect("private test file");
        file.write_all(contents).expect("write private test file");
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    engine: Engine,
}

fn open_engine(config_text: &str) -> Harness {
    let directory = private_tempdir();
    let config_root = directory.path().join("config");
    create_private_test_dir(&config_root);
    write_private_test_file(&config_root.join("config.toml"), config_text.as_bytes());
    let config = load_from_roots(None, Some(&config_root)).expect("loaded config");
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);
    let now = Timestamp::now();
    let catalog = Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "0".repeat(64)))
            .expect("catalog revision"),
        source: CatalogSource::Bootstrap,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Bootstrap,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::new(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    });
    let manager = Arc::new(
        ModelManager::new(
            BTreeMap::new(),
            catalog,
            ProviderStore::open(provider_store).expect("provider store"),
        )
        .expect("model manager"),
    );
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("engine");
    Harness {
        _directory: directory,
        engine,
    }
}

async fn wait_for_state(engine: &Engine, plugin: &str, expected: PluginState) {
    // Plugin lifecycle state has no public subscription; integration tests can
    // only observe the public status snapshot.
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if engine
                .plugin_statuses()
                .into_iter()
                .any(|status| status.plugin == plugin && status.state == expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "plugin state timeout waiting for {plugin:?}={expected:?}; statuses={:?}",
        engine.plugin_statuses()
    );
}

fn status(engine: &Engine, plugin: &str) -> cookie_agent_engine::PluginStatus {
    engine
        .plugin_statuses()
        .into_iter()
        .find(|status| status.plugin == plugin)
        .expect("plugin status")
}

async fn wait_for_mcp_connected(engine: &Engine, server: &str) {
    // MCP lifecycle state likewise has no public transition subscription.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if engine.mcp_statuses().into_iter().any(|status| {
                status.server == server
                    && status.state == cookie_agent_engine::McpServerState::Connected
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("MCP state timeout");
}

// Process reaping is observed through Linux procfs.
#[cfg(target_os = "linux")]
async fn assert_process_reaped(pid_file: &std::path::Path) {
    let pid = fs::read_to_string(pid_file).expect("plugin pid");
    let proc_path = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    tokio::time::timeout(Duration::from_secs(3), async {
        while proc_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("plugin process was not reaped");
}

#[tokio::test]
async fn happy_path_discovers_claims_pings_and_shuts_down() {
    let marker_directory = tempfile::tempdir().expect("marker directory");
    let path = marker_directory.path().join("shutdown");
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            ("FIXTURE_TOOLS", tools("fixture_tool")),
            ("FIXTURE_SHUTDOWN_FILE", path.display().to_string()),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "fixture", PluginState::Connected).await;
    assert_eq!(status(&harness.engine, "fixture").tools, ["fixture_tool"]);
    harness.engine.ping_plugin("fixture").await.expect("ping");
    harness.engine.shutdown().await;
    assert_eq!(
        fs::read_to_string(path).expect("shutdown marker"),
        "shutdown"
    );
}

// This test specifically asserts Unix HOME sanitization in the child environment.
#[cfg(unix)]
#[tokio::test]
async fn plugin_environment_contains_only_configured_values() {
    assert!(
        std::env::var_os("HOME").is_some(),
        "test parent must have HOME"
    );
    let marker_directory = tempfile::tempdir().expect("marker directory");
    let env_file = marker_directory.path().join("environment.json");
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            ("FIXTURE_ENV_FILE", env_file.display().to_string()),
            ("FIXTURE_CONFIGURED_SENTINEL", "configured-value".into()),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "fixture", PluginState::Connected).await;
    let environment: serde_json::Value =
        serde_json::from_slice(&fs::read(env_file).expect("fixture environment"))
            .expect("environment JSON");
    assert_eq!(environment["parent"], serde_json::Value::Null);
    assert_eq!(environment["configured"], "configured-value");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn ping_dispatches_interleaved_notifications_and_requests() {
    let marker_directory = tempfile::tempdir().expect("marker directory");
    let rejected = marker_directory.path().join("request-rejected");
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            ("FIXTURE_INTERLEAVE_PING", "1".into()),
            (
                "FIXTURE_REQUEST_REJECTED_FILE",
                rejected.display().to_string(),
            ),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "fixture", PluginState::Connected).await;
    harness.engine.ping_plugin("fixture").await.expect("ping");
    // The fixture reports this protocol response through an external marker
    // file, so there is no in-process event to subscribe to.
    tokio::time::timeout(Duration::from_secs(1), async {
        while !rejected.exists()
            || fs::read_to_string(&rejected).is_ok_and(|contents| contents.is_empty())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("plugin request rejection");
    assert_eq!(fs::read_to_string(rejected).expect("rejection"), "-32601");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn exact_version_mismatch_fails_without_stopping_engine() {
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            ("FIXTURE_PROTOCOL_VERSION", "0.0.3".into()),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "fixture", PluginState::Failed).await;
    assert!(
        status(&harness.engine, "fixture")
            .reason
            .unwrap()
            .contains("0.0.3")
    );
    harness
        .engine
        .runtime_snapshot()
        .expect("engine remains usable");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn timeout_and_malformed_handshakes_fail_without_panicking() {
    let pid_directory = tempfile::tempdir().expect("pid directory");
    let slow_pid = pid_directory.path().join("slow.pid");
    let malformed_pid = pid_directory.path().join("malformed.pid");
    let config = format!(
        "{}{}",
        plugin_table(
            "slow",
            &[
                ("FIXTURE_NAME", "slow".into()),
                ("FIXTURE_DELAY_MS", "1000".into()),
                ("FIXTURE_PID_FILE", slow_pid.display().to_string()),
            ],
            "startup_timeout_ms = 50",
        ),
        plugin_table(
            "malformed",
            &[
                ("FIXTURE_NAME", "malformed".into()),
                ("FIXTURE_MALFORMED", "1".into()),
                ("FIXTURE_PID_FILE", malformed_pid.display().to_string()),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "slow", PluginState::Failed).await;
    wait_for_state(&harness.engine, "malformed", PluginState::Failed).await;
    assert!(
        status(&harness.engine, "slow")
            .reason
            .unwrap()
            .contains("timeout")
    );
    assert!(
        status(&harness.engine, "malformed")
            .reason
            .unwrap()
            .contains("malformed")
    );
    #[cfg(target_os = "linux")]
    {
        assert_process_reaped(&slow_pid).await;
        assert_process_reaped(&malformed_pid).await;
    }
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn oversized_and_invalid_utf8_frames_fail_plugins() {
    let config = format!(
        "{}{}",
        plugin_table(
            "oversized",
            &[
                ("FIXTURE_NAME", "oversized".into()),
                ("FIXTURE_OVERSIZED_AFTER_INITIALIZE", "1".into()),
            ],
            "",
        ),
        plugin_table(
            "invalid_utf8",
            &[
                ("FIXTURE_NAME", "invalid_utf8".into()),
                ("FIXTURE_INVALID_UTF8_AFTER_INITIALIZE", "1".into()),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "oversized", PluginState::Failed).await;
    wait_for_state(&harness.engine, "invalid_utf8", PluginState::Failed).await;
    assert!(
        status(&harness.engine, "oversized")
            .reason
            .unwrap()
            .contains("4194304")
    );
    assert!(
        status(&harness.engine, "invalid_utf8")
            .reason
            .unwrap()
            .contains("UTF-8")
    );
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn malformed_schema_and_unknown_primary_resource_fail_plugins() {
    let malformed_schemas = [
        ("bad_type", r#"{"type":42}"#),
        ("bad_length_type", r#"{"type":"string","minLength":"bad"}"#),
        ("bad_negative_length", r#"{"type":"string","minLength":-1}"#),
        ("bad_enum", r#"{"enum":"bad"}"#),
        ("bad_numeric", r#"{"type":"number","multipleOf":0}"#),
    ];
    let unknown_resource = r#"[{"name":"bad_resource","description":"bad","parameters":{"type":"object","properties":{}},"permission_name":"bad_resource","primary_resource_param":"path"}]"#;
    let mut config = malformed_schemas
        .iter()
        .map(|(name, parameters)| {
            let declaration = format!(
                r#"[{{"name":"{name}","description":"bad","parameters":{parameters},"permission_name":"{name}","primary_resource_param":null}}]"#
            );
            plugin_table(
                name,
                &[
                    ("FIXTURE_NAME", (*name).into()),
                    ("FIXTURE_TOOLS", declaration),
                ],
                "",
            )
        })
        .collect::<String>();
    config.push_str(&plugin_table(
        "bad_resource",
        &[
            ("FIXTURE_NAME", "bad_resource".into()),
            ("FIXTURE_TOOLS", unknown_resource.into()),
        ],
        "",
    ));
    let harness = open_engine(&config);
    for (name, _) in malformed_schemas {
        wait_for_state(&harness.engine, name, PluginState::Failed).await;
        assert!(
            status(&harness.engine, name)
                .reason
                .unwrap()
                .contains("JSON Schema")
        );
    }
    wait_for_state(&harness.engine, "bad_resource", PluginState::Failed).await;
    assert!(
        status(&harness.engine, "bad_resource")
            .reason
            .unwrap()
            .contains("primary_resource_param")
    );
    harness.engine.shutdown().await;
}

// This test terminates the fixture with the Unix `kill -KILL` command.
#[cfg(unix)]
#[tokio::test]
async fn crash_clears_claims_and_leaves_other_plugin_connected() {
    let pid_directory = tempfile::tempdir().expect("pid directory");
    let pid_file = pid_directory.path().join("crasher.pid");
    let config = format!(
        "{}{}",
        plugin_table(
            "crasher",
            &[
                ("FIXTURE_NAME", "crasher".into()),
                ("FIXTURE_TOOLS", tools("released_tool")),
                ("FIXTURE_PID_FILE", pid_file.display().to_string()),
            ],
            "",
        ),
        plugin_table(
            "steady",
            &[
                ("FIXTURE_NAME", "steady".into()),
                ("FIXTURE_TOOLS", tools("steady_tool")),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "crasher", PluginState::Connected).await;
    wait_for_state(&harness.engine, "steady", PluginState::Connected).await;
    let pid = fs::read_to_string(pid_file).expect("plugin pid");
    let exit = std::process::Command::new("kill")
        .args(["-KILL", pid.trim()])
        .status()
        .expect("kill plugin");
    assert!(exit.success());
    wait_for_state(&harness.engine, "crasher", PluginState::Failed).await;
    assert!(status(&harness.engine, "crasher").tools.is_empty());
    harness
        .engine
        .runtime_snapshot()
        .expect("engine remains usable");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn built_in_tool_collision_fails_only_that_plugin() {
    let config = format!(
        "{}{}",
        plugin_table(
            "collision",
            &[
                ("FIXTURE_NAME", "collision".into()),
                ("FIXTURE_TOOLS", tools("read")),
            ],
            "",
        ),
        plugin_table(
            "steady",
            &[
                ("FIXTURE_NAME", "steady".into()),
                ("FIXTURE_TOOLS", tools("steady_tool")),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "collision", PluginState::Failed).await;
    wait_for_state(&harness.engine, "steady", PluginState::Connected).await;
    assert!(
        status(&harness.engine, "collision")
            .reason
            .unwrap()
            .contains("colliding")
    );
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn skill_tool_collision_fails_plugin_without_breaking_composition() {
    let config = plugin_table(
        "collision",
        &[
            ("FIXTURE_NAME", "collision".into()),
            ("FIXTURE_TOOLS", tools("skill")),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "collision", PluginState::Failed).await;
    assert!(
        status(&harness.engine, "collision")
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("colliding tool name `skill`"))
    );
    harness
        .engine
        .runtime_snapshot()
        .expect("engine remains composed");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn eager_mcp_preempts_plugin_claim_regardless_of_startup_order() {
    let config = format!(
        "{}{}",
        mcp_table("fixture", false),
        plugin_table(
            "collision",
            &[
                ("FIXTURE_NAME", "collision".into()),
                ("FIXTURE_TOOLS", tools("fixture_echo_text")),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_mcp_connected(&harness.engine, "fixture").await;
    wait_for_state(&harness.engine, "collision", PluginState::Failed).await;
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn later_plugin_registration_wins_only_the_duplicate_tool() {
    let first_tools = r#"[{"name":"shared_tool","description":"Shared","parameters":{"type":"object","properties":{}},"permission_name":"first_permission","primary_resource_param":null},{"name":"first_only","description":"First only","parameters":{"type":"object","properties":{}},"permission_name":"first_only","primary_resource_param":null}]"#;
    let config = format!(
        "{}{}",
        plugin_table(
            "first",
            &[
                ("FIXTURE_NAME", "first".into()),
                ("FIXTURE_TOOLS", first_tools.into()),
            ],
            "",
        ),
        plugin_table(
            "second",
            &[
                ("FIXTURE_NAME", "second".into()),
                ("FIXTURE_TOOLS", tools("shared_tool")),
                ("FIXTURE_DELAY_MS", "100".into()),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "first", PluginState::Connected).await;
    wait_for_state(&harness.engine, "second", PluginState::Connected).await;
    assert_eq!(status(&harness.engine, "first").tools, ["first_only"]);
    assert!(
        status(&harness.engine, "first")
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("later plugin `second`"))
    );
    assert_eq!(status(&harness.engine, "second").tools, ["shared_tool"]);
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn lazy_mcp_preempts_an_existing_plugin_claim_when_connected() {
    let config = format!(
        "{}{}",
        mcp_table("fixture", true),
        plugin_table(
            "collision",
            &[
                ("FIXTURE_NAME", "collision".into()),
                ("FIXTURE_TOOLS", tools("fixture_echo_text")),
            ],
            "",
        )
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "collision", PluginState::Connected).await;
    harness
        .engine
        .reconnect_mcp_server("fixture".into())
        .await
        .expect("connect lazy MCP");
    wait_for_mcp_connected(&harness.engine, "fixture").await;
    wait_for_state(&harness.engine, "collision", PluginState::Failed).await;
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn shutdown_during_pending_initialize_is_bounded() {
    let pid_directory = tempfile::tempdir().expect("pid directory");
    let pid_file = pid_directory.path().join("slow.pid");
    let config = plugin_table(
        "slow",
        &[
            ("FIXTURE_NAME", "slow".into()),
            ("FIXTURE_DELAY_MS", "5000".into()),
            ("FIXTURE_PID_FILE", pid_file.display().to_string()),
        ],
        "startup_timeout_ms = 10000\nshutdown_grace_ms = 50",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "slow", PluginState::Connecting).await;
    tokio::time::timeout(Duration::from_secs(1), harness.engine.shutdown())
        .await
        .expect("bounded shutdown");
    #[cfg(target_os = "linux")]
    assert_process_reaped(&pid_file).await;
}
