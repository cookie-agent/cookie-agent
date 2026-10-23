//! Interrupting a run (`run.cancel`) while its tool calls run: every
//! foreground call ends promptly as cancelled, and the run's terminal event
//! does not wait for the call's own work to finish.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use cookie_agent_config::{LoadedMcpServer, McpServerConfig, McpServerSource};
use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, RunId, RunStartParams, SessionId, SessionStatus,
    ToolTerminationOutcome,
};

use super::support::*;
use crate::{Engine, EngineOptions};

/// How long an interrupt may take to end a run whose calls stop when asked.
// Windows CI runners are slow enough to cross 1.5 s now and then.
const PROMPT: Duration = Duration::from_millis(if cfg!(windows) { 3_000 } else { 1_500 });

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

/// `fixture` reopened with the coding runtime as its `local` runtime,
/// behind the runtime protocol as the daemon runs it.
async fn with_protocol_local_runtime(fixture: Fixture) -> Fixture {
    fixture.engine.shutdown().await;
    let directory = fixture._directory.path().to_owned();
    let engine = Engine::open_async(EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.clone(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions {
            local: cookie_agent_runtime_api::LocalRuntime::Protocol(
                cookie_agent_coding_runtime::server::in_process_connector(),
            ),
            ..cookie_agent_runtime_api::RuntimeOptions::new(&directory)
        },
    })
    .await
    .expect("engine with a protocol local runtime");
    Fixture { engine, ..fixture }
}

/// A primary agent allowed `permissions`.
fn primary(permissions: &str) -> String {
    format!(
        "---\ndescription: Interrupt test agent\nmode: primary\nenabled: true\nmodels: [{{ model: \"custom.test/group/model\", variant: base }}]\npermissions:\n{permissions}\n---\nInterrupt test.\n"
    )
}

/// Starts a run of a new session of `engine` whose first model turn calls
/// `tool` with `arguments`.
async fn start_tool_run(
    engine: &Engine,
    selection: cookie_agent_protocol::RunSelection,
    session: SessionId,
) -> RunId {
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("interrupt").unwrap(),
                selection,
                content: "start the call".into(),
            },
            origin(),
        )
        .await
        .expect("run started")
        .run_id
}

/// Waits until `path` exists, as a command's marker that it is running.
async fn wait_for_file(path: &Path) {
    with_watchdog("the call to start", async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// Cancels `run` of `session` and returns how long its terminal event took,
/// with the termination of its one call.
async fn interrupt(
    engine: &Engine,
    session: SessionId,
    run: RunId,
) -> (Duration, cookie_agent_protocol::ToolCallTermination) {
    let started = Instant::now();
    engine.cancel_run(run, origin()).await.expect("cancel run");
    await_event(engine, session, "the run's cancellation", |event| {
        event.run_id == Some(run)
            && matches!(
                &event.payload,
                EventPayload::RunFinished {
                    outcome: cookie_agent_protocol::Open::Known(
                        cookie_agent_protocol::RunOutcome::Cancelled { .. }
                    )
                }
            )
    })
    .await;
    let elapsed = started.elapsed();
    let projection = engine.inner.store.get(session).unwrap();
    let terminations = projection
        .log
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } if event.run_id == Some(run) => {
                Some(termination.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminations.len(), 1, "{terminations:?}");
    (elapsed, terminations.into_iter().next().unwrap())
}

/// Whether process `pid` is gone (or a zombie nobody reaped yet).
#[cfg(target_os = "linux")]
fn process_gone(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Waits until the process whose ID `path` holds is gone.
#[cfg(target_os = "linux")]
async fn wait_for_process_gone(path: &Path) {
    let pid = std::fs::read_to_string(path).unwrap();
    let pid = pid.trim().to_owned();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !process_gone(&pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} still runs"));
}

/// A bash call through the protocol `local` runtime that runs `command`.
async fn local_bash_run(command: &str) -> (Fixture, SessionId, RunId) {
    let (endpoint, responses, _server) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body("bash-call", "bash", serde_json::json!({"command": command})),
        ))
        .unwrap();
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &primary("  bash: allow"));
    let fixture = with_protocol_local_runtime(fixture).await;
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let run = start_tool_run(&fixture.engine, selection, session).await;
    (fixture, session, run)
}

fn marker(directory: &Path, name: &str) -> PathBuf {
    directory.join(name)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_kills_a_local_bash_tree_at_once() {
    let markers = private_tempdir();
    let ready = marker(markers.path(), "ready");
    let shell = marker(markers.path(), "shell");
    let child = marker(markers.path(), "child");
    let command = format!(
        "sleep 60 & echo $! > '{}'; echo $$ > '{}'; touch '{}'; wait",
        child.display(),
        shell.display(),
        ready.display()
    );
    let (fixture, session, run) = local_bash_run(&command).await;
    wait_for_file(&ready).await;
    let (elapsed, termination) = interrupt(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: local bash `sleep 60`: {elapsed:?}");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
    assert!(elapsed < PROMPT, "{elapsed:?}");
    #[cfg(target_os = "linux")]
    {
        wait_for_process_gone(&shell).await;
        wait_for_process_gone(&child).await;
    }
    fixture.engine.shutdown().await;
}

/// A process that left the command's process group keeps its output pipes
/// open after the group is killed; the call must not wait for it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_does_not_wait_for_a_process_that_escaped_the_group() {
    let markers = private_tempdir();
    let ready = marker(markers.path(), "ready");
    let escaped = marker(markers.path(), "escaped");
    let command = format!(
        "setsid sleep 60 & echo $! > '{}'; touch '{}'; wait",
        escaped.display(),
        ready.display()
    );
    let (fixture, session, run) = local_bash_run(&command).await;
    wait_for_file(&ready).await;
    let (elapsed, termination) = interrupt(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: local bash with an escaped pipe holder: {elapsed:?}");
    if let Ok(pid) = std::fs::read_to_string(&escaped) {
        let _ = std::process::Command::new("kill")
            .args(["-9", pid.trim()])
            .status();
    }
    assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
    let message = termination.error.unwrap().message.to_string();
    assert!(!message.contains("cleanup deadline"), "{message}");
    assert!(elapsed < PROMPT, "{elapsed:?}");
    fixture.engine.shutdown().await;
}

/// A webfetch whose server accepts the connection and never answers.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_stops_a_webfetch_waiting_on_its_server() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = silent.local_addr().unwrap();
    let accepted = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&accepted);
    let _silent = std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in silent.incoming().flatten() {
            held.push(stream);
            signal.notify_one();
        }
    });
    let (endpoint, responses, _server) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "webfetch-call",
                "webfetch",
                serde_json::json!({"url": format!("http://{address}/slow")}),
            ),
        ))
        .unwrap();
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &primary("  webfetch: allow"));
    let fixture = with_protocol_local_runtime(fixture).await;
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let run = start_tool_run(&fixture.engine, selection, session).await;
    with_watchdog("the fetch to connect", accepted.notified()).await;
    let (elapsed, termination) = interrupt(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: webfetch on a silent server: {elapsed:?}");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
    let message = termination.error.unwrap().message.to_string();
    assert!(!message.contains("cleanup deadline"), "{message}");
    assert!(elapsed < PROMPT, "{elapsed:?}");
    fixture.engine.shutdown().await;
}

fn hanging_mcp_server(cancel_file: &Path) -> LoadedMcpServer {
    let mut env = std::collections::BTreeMap::new();
    env.insert(
        "MCP_FIXTURE_CANCEL_FILE".into(),
        cancel_file.display().to_string(),
    );
    LoadedMcpServer {
        source: McpServerSource::UserFile,
        config: McpServerConfig {
            command: Some(python_command().into()),
            args: vec![format!(
                "{}/../mcp/tests/fixtures/mcp_server.py",
                env!("CARGO_MANIFEST_DIR")
            )],
            env,
            cwd: None,
            url: None,
            headers: std::collections::BTreeMap::new(),
            oauth: Default::default(),
            enabled: true,
            timeout_ms: Some(60_000),
            scope: None,
            include_tools: None,
            exclude_tools: Vec::new(),
        },
    }
}

/// An MCP call the server never answers ends at once, and the server hears
/// `notifications/cancelled` for it.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_cancels_an_mcp_call_in_flight() {
    let markers = private_tempdir();
    let cancel_file = marker(markers.path(), "cancelled.jsonl");
    let (endpoint, responses, _server) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "mcp-call",
                "fixture_echo_text",
                serde_json::json!({"text": "hang"}),
            ),
        ))
        .unwrap();
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        &primary("  mcp: allow"),
        None,
        None,
        false,
        None,
        Some(hanging_mcp_server(&cancel_file)),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let run = start_tool_run(&fixture.engine, selection, session).await;
    await_event(&fixture.engine, session, "the MCP call to start", |event| {
        matches!(event.payload, EventPayload::ToolCallStarted { .. })
    })
    .await;
    // The request is on its way to the server once the call executes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (elapsed, termination) = interrupt(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: MCP call in flight: {elapsed:?}");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
    assert!(elapsed < PROMPT, "{elapsed:?}");
    let notified = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&cancel_file)
                && !text.is_empty()
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server hears notifications/cancelled");
    let params: serde_json::Value = serde_json::from_str(notified.lines().next().unwrap()).unwrap();
    assert!(params["requestId"].is_number() || params["requestId"].is_string());
    fixture.engine.shutdown().await;
}

/// A foreground delegation ends as cancelled at once; its child keeps
/// running (background tool calls spec, section 7.4).
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_ends_a_foreground_delegation_at_once_and_leaves_the_child_running() {
    // The child's model request never gets an answer.
    let (endpoint, responses, server) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "start the call",
            scripted_tool_body(
                "delegate-call",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker",
                    "description": "Waiting child",
                    "prompt": "child waits on its model"
                }),
            ),
        ))
        .unwrap();
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let run = start_tool_run(&fixture.engine, selection, session).await;
    let child = await_child(&fixture.engine, session, "the running child", |child| {
        child.status == SessionStatus::Running
    })
    .await
    .session_id;
    let (elapsed, termination) = interrupt(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: foreground delegation: {elapsed:?}");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
    assert!(elapsed < PROMPT, "{elapsed:?}");
    assert_eq!(
        fixture.engine.inner.store.get(child).unwrap().status,
        SessionStatus::Running,
        "the child keeps running"
    );
    server.abort();
    fixture.engine.shutdown().await;
}
