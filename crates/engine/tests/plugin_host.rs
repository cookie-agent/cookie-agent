use std::{collections::BTreeMap, fs, sync::Arc, time::Duration};

use cookie_agent_config::load_from_user_root;
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_identity::CatalogRevision;
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{PluginInfo, PluginState};
use jiff::Timestamp;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");

fn tools(name: &str) -> String {
    format!(
        r#"[{{"name":"{name}","description":"Fixture tool","parameters":{{"type":"object","properties":{{}}}}}}]"#
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
        "[plugins.{name}]\ncommand = [{}, {}]\nenv = {{ {environment} }}\n{extra}\n",
        toml_string(python_command()),
        toml_string(FIXTURE)
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
    let config = load_from_user_root(Some(&config_root)).expect("loaded config");
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
        providers: Arc::new(BTreeMap::new()),
        canonical_models: Arc::new(BTreeMap::new()),
        quarantine: Arc::new(Vec::new()),
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
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory.path()),
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
                .any(|status| status.name == plugin && status.state == expected)
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

fn status(engine: &Engine, plugin: &str) -> PluginInfo {
    engine
        .plugin_statuses()
        .into_iter()
        .find(|status| status.name == plugin)
        .expect("plugin status")
}

/// The fixture publishes its pid atomically once Python is running, so a
/// missing file means the plugin was killed before it got that far.
fn read_pid(pid_file: &std::path::Path) -> Option<u32> {
    let pid = fs::read_to_string(pid_file).ok()?;
    Some(pid.trim().parse().expect("fixture pid is a number"))
}

async fn wait_for_pid(pid_file: &std::path::Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(pid) = read_pid(pid_file) {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("plugin never published its pid")
}

// Process reaping is observed through Linux procfs.
#[cfg(target_os = "linux")]
async fn assert_process_reaped(pid: u32) {
    let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
    tokio::time::timeout(Duration::from_secs(3), async {
        while proc_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("plugin process was not reaped");
}

#[tokio::test]
async fn a_plugin_connects_declares_its_tools_and_shuts_down() {
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
    harness.engine.shutdown().await;
    assert_eq!(
        fs::read_to_string(path).expect("shutdown marker"),
        "shutdown"
    );
    assert_eq!(
        status(&harness.engine, "fixture").state,
        PluginState::Stopped
    );
}

// Plugins inherit the engine's environment; `env` adds to it and wins.
#[cfg(unix)]
#[tokio::test]
async fn plugin_environment_inherits_the_engine_and_adds_configured_values() {
    let parent_home = std::env::var("HOME").expect("test parent must have HOME");
    for configured_home in [None, Some("/configured-home")] {
        let marker_directory = tempfile::tempdir().expect("marker directory");
        let env_file = marker_directory.path().join("environment.json");
        let mut variables = vec![
            ("FIXTURE_NAME", "fixture".to_owned()),
            ("FIXTURE_ENV_FILE", env_file.display().to_string()),
            ("FIXTURE_CONFIGURED_SENTINEL", "configured-value".into()),
        ];
        if let Some(home) = configured_home {
            variables.push(("HOME", home.into()));
        }
        let config = plugin_table("fixture", &variables, "");
        let harness = open_engine(&config);
        wait_for_state(&harness.engine, "fixture", PluginState::Connected).await;
        let environment: serde_json::Value =
            serde_json::from_slice(&fs::read(env_file).expect("fixture environment"))
                .expect("environment JSON");
        assert_eq!(
            environment["parent"],
            configured_home.unwrap_or(parent_home.as_str())
        );
        assert_eq!(environment["configured"], "configured-value");
        harness.engine.shutdown().await;
    }
}

#[tokio::test]
async fn a_plugin_of_another_version_fails_with_unsupported_protocol() {
    let daemon = cookie_agent_protocol::PLUGIN_PROTOCOL_VERSION;
    let next_minor = cookie_agent_protocol::SemVer::new(daemon.major(), daemon.minor() + 1, 0);
    let next_major = cookie_agent_protocol::SemVer::new(daemon.major() + 1, 0, 0);
    for peer in [
        format!("\"{next_minor}\""),
        format!("\"{next_major}\""),
        // A build from before semver versions.
        "2".to_owned(),
    ] {
        let config = plugin_table(
            "fixture",
            &[
                ("FIXTURE_NAME", "fixture".into()),
                ("FIXTURE_PROTOCOL_VERSION", peer.clone()),
            ],
            "",
        );
        let harness = open_engine(&config);
        wait_for_state(&harness.engine, "fixture", PluginState::Reconnecting).await;
        let reason = status(&harness.engine, "fixture").reason.unwrap();
        let named = peer.trim_matches('"');
        assert!(
            reason.contains("plugin `fixture`")
                && reason.contains(&format!("{daemon} here and {named}")),
            "{reason}"
        );
        harness
            .engine
            .runtime_snapshot()
            .expect("engine remains usable");
        harness.engine.shutdown().await;
    }
}

#[tokio::test]
async fn a_plugin_of_another_patch_connects_and_unknown_features_are_noted() {
    let daemon = cookie_agent_protocol::PLUGIN_PROTOCOL_VERSION;
    let patch =
        cookie_agent_protocol::SemVer::new(daemon.major(), daemon.minor(), daemon.patch() + 3);
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            ("FIXTURE_PROTOCOL_VERSION", format!("\"{patch}\"")),
            ("FIXTURE_FEATURES", r#"["time_travel"]"#.into()),
            ("FIXTURE_TOOLS", tools("fixture_tool")),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "fixture", PluginState::Connected).await;
    let connected = status(&harness.engine, "fixture");
    assert_eq!(connected.tools, ["fixture_tool"]);
    assert!(
        connected
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("feature `time_travel`")),
        "{connected:?}"
    );
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
    wait_for_state(&harness.engine, "slow", PluginState::Reconnecting).await;
    wait_for_state(&harness.engine, "malformed", PluginState::Reconnecting).await;
    let slow = status(&harness.engine, "slow").reason.unwrap();
    assert!(slow.contains("did not start within 50 ms"), "{slow}");
    let malformed = status(&harness.engine, "malformed").reason.unwrap();
    assert!(malformed.contains("handshake failed"), "{malformed}");
    #[cfg(target_os = "linux")]
    {
        // A loaded machine can hit the 50 ms handshake timeout before Python
        // starts; that plugin never ran far enough to publish a pid.
        if let Some(pid) = read_pid(&slow_pid) {
            assert_process_reaped(pid).await;
        }
        assert_process_reaped(read_pid(&malformed_pid).expect("malformed plugin pid")).await;
    }
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn a_dropped_plugin_reconnects_and_reports_what_it_wrote_to_stderr() {
    let config = plugin_table(
        "crashing",
        &[
            ("FIXTURE_NAME", "crashing".into()),
            ("FIXTURE_STDERR", "fixture lost its database".into()),
            ("FIXTURE_CRASH_AFTER_INITIALIZE", "1".into()),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "crashing", PluginState::Reconnecting).await;
    let reason = status(&harness.engine, "crashing").reason.unwrap();
    assert!(reason.contains("fixture lost its database"), "{reason}");
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
    wait_for_state(&harness.engine, "slow", PluginState::Starting).await;
    // `Starting` is published before the spawn; the pid proves Python is
    // running and parked in its delayed initialize.
    let pid = wait_for_pid(&pid_file).await;
    tokio::time::timeout(Duration::from_secs(2), harness.engine.shutdown())
        .await
        .expect("bounded shutdown");
    #[cfg(target_os = "linux")]
    assert_process_reaped(pid).await;
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[tokio::test]
async fn unusable_intercept_entries_and_event_patterns_are_skipped_with_a_diagnostic() {
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            (
                "FIXTURE_CAPABILITIES",
                r#"{"producer_messaging":false,"subscribe_events":["run_*","Run Anything"],
                    "subscribe_bus":false,
                    "publish_bus":false,"intercept":[
                        {"hook":"tool_before_call","on_error":"block","since":"0.9.0"},
                        {"hook":"tool_teleport"},
                        "message_end",
                        {"hook":"tool_before_call"}
                    ],
                    "subscribe_dreams":true}"#
                    .into(),
            ),
        ],
        "",
    );
    let harness = open_engine(&config);
    wait_for_state(&harness.engine, "fixture", PluginState::Connected).await;
    let connected = status(&harness.engine, "fixture");
    assert_eq!(
        connected
            .hooks
            .iter()
            .map(cookie_agent_protocol::HookName::as_str)
            .collect::<Vec<_>>(),
        ["tool_before_call"]
    );
    assert_eq!(
        connected
            .subscribe_events
            .iter()
            .map(cookie_agent_protocol::EventPattern::as_str)
            .collect::<Vec<_>>(),
        ["run_*"]
    );
    let reason = connected.reason.as_deref().unwrap_or_default();
    for expected in [
        "hook `tool_teleport`, which this daemon does not know",
        "an intercept entry this daemon cannot read",
        "hook `tool_before_call` more than once",
        "events by \"Run Anything\", which this daemon cannot read",
    ] {
        assert!(
            reason.contains(expected),
            "{expected:?} missing: {connected:?}"
        );
    }
    harness.engine.shutdown().await;
}

/// What a plugin sends that the daemon cannot take is reported, and the
/// plugin stays connected: a runtime-only declaration, and commands with a
/// member the daemon does not know.
#[tokio::test]
async fn messages_a_plugin_may_not_send_are_reported_and_ignored() {
    let session = cookie_agent_protocol::SessionId::new_v7();
    let tools_changed = serde_json::json!({"jsonrpc": "2.0", "method": "capability/tools_changed",
        "params": {"instance": "i", "tools": {"tools": []}}});
    let mcp_changed = serde_json::json!({"jsonrpc": "2.0", "method": "mcp/changed",
        "params": {"instance": "i", "servers": []}});
    let emit = serde_json::json!({"jsonrpc": "2.0", "method": "plugin/emit", "params": {
        "session_id": session, "context_id": "c", "name": "n", "payload": {}, "priority": 1}});
    for (frame, expected) in [
        (
            tools_changed,
            "ignored `capability/tools_changed`: only a runtime sends it",
        ),
        (
            mcp_changed,
            "ignored `mcp/changed`: only a runtime sends it",
        ),
        (
            emit,
            "malformed emit: The request has a member the receiver does not know: priority.",
        ),
    ] {
        let config = plugin_table(
            "fixture",
            &[
                ("FIXTURE_NAME", "fixture".into()),
                (
                    "FIXTURE_AFTER_INITIALIZE",
                    serde_json::json!([frame]).to_string(),
                ),
            ],
            "",
        );
        let harness = open_engine(&config);
        let reported = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let current = status(&harness.engine, "fixture");
                if current.state == PluginState::Connected
                    && current.reason.as_deref() == Some(expected)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            reported.is_ok(),
            "{expected}: {:?}",
            status(&harness.engine, "fixture")
        );
        harness.engine.shutdown().await;
    }
}

/// A command request with a member the daemon does not know is refused
/// with `-32602` and the member's JSON path.
#[tokio::test]
async fn a_command_with_an_unknown_member_is_refused_with_its_path() {
    let directory = tempfile::tempdir().expect("reply directory");
    let replies = directory.path().join("replies.jsonl");
    let progress = serde_json::json!({"jsonrpc": "2.0", "id": 900, "method": "tool/progress",
        "params": {"prepared_id": 1, "progress": {
            "tool_call_id": cookie_agent_protocol::ToolCallId::new_v7(),
            "message": "working", "eta_ms": 5}}});
    let config = plugin_table(
        "fixture",
        &[
            ("FIXTURE_NAME", "fixture".into()),
            (
                "FIXTURE_AFTER_INITIALIZE",
                serde_json::json!([progress]).to_string(),
            ),
            ("FIXTURE_REPLY_FILE", replies.display().to_string()),
        ],
        "",
    );
    let harness = open_engine(&config);
    let reply: serde_json::Value = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(text) = fs::read_to_string(&replies)
                && let Some(line) = text.lines().next()
            {
                break serde_json::from_str(line).expect("reply JSON");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the daemon answers");
    assert_eq!(reply["id"], 900);
    assert_eq!(reply["error"]["code"], -32602);
    assert_eq!(reply["error"]["data"]["code"], "unknown_field");
    assert_eq!(reply["error"]["data"]["field"], "progress.eta_ms");
    assert_eq!(
        status(&harness.engine, "fixture").state,
        PluginState::Connected
    );
    harness.engine.shutdown().await;
}
