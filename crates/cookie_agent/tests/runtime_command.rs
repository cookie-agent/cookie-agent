//! `cookie runtime serve --stdio`: the real binary serving the runtime
//! protocol over its standard input and output.

use std::{path::Path, process::Stdio, time::Duration};

use cookie_agent_protocol::RUNTIME_PROTOCOL_VERSION;
use cookie_agent_runtime_api::{
    protocol::{
        AgentMdLimits, CapabilityOpenParams, CapabilityOpenResult, Empty, InitializeParams,
        InitializeResult, RuntimeSettings, ToolOutputLimits, method,
    },
    rpc::{LineTransport, Peer},
};
use tokio::process::{Child, Command};

const WAIT: Duration = Duration::from_secs(30);

fn runtime(home: &Path, roots: &[&str]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
    command.args(["runtime", "serve", "--stdio"]);
    for root in roots {
        command.args(["--root", root]);
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn cookie runtime")
}

fn connect(child: &mut Child) -> Peer {
    let transport = LineTransport::new(
        child.stdout.take().expect("stdout"),
        child.stdin.take().expect("stdin"),
    );
    let (peer, _incoming) = Peer::start(Box::new(transport));
    peer
}

fn params() -> InitializeParams {
    InitializeParams {
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        engine_version: "test".into(),
        features: Vec::new(),
        runtime_id: "labpc".into(),
        settings: RuntimeSettings {
            tool_output: ToolOutputLimits {
                max_lines: 100,
                max_bytes: 10_000,
                head_lines: 50,
                tail_lines: 50,
            },
            agent_md: AgentMdLimits { max_bytes: 4096 },
            mcp_servers: Vec::new(),
            acp_agents: Vec::new(),
        },
    }
}

fn open(workspace: &str) -> CapabilityOpenParams {
    CapabilityOpenParams {
        capability: cookie_agent_protocol::CapabilityName::coding(),
        workspace: Some(workspace.to_owned()),
    }
}

async fn initialize(peer: &Peer) -> InitializeResult {
    tokio::time::timeout(WAIT, peer.request(method::INITIALIZE, &params()))
        .await
        .expect("initialize answers")
        .expect("initialize succeeds")
}

async fn exits_successfully(child: &mut Child) {
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("the runtime exits")
        .expect("exit status");
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn the_runtime_opens_workspaces_expanding_home_and_stops_on_shutdown() {
    let home = tempfile::tempdir().unwrap();
    let repository = home.path().join("My Repo");
    std::fs::create_dir(&repository).unwrap();
    let mut child = runtime(home.path(), &[]);
    let peer = connect(&mut child);
    let initialized = initialize(&peer).await;
    assert_eq!(
        initialized
            .capabilities
            .iter()
            .map(|capability| capability.name.as_str())
            .collect::<Vec<_>>(),
        ["coding"]
    );

    // The daemon may name the workspace in its own spelling; the runtime
    // resolves it on its machine and reports the canonical path.
    let opened: CapabilityOpenResult = tokio::time::timeout(
        WAIT,
        peer.request(method::CAPABILITY_OPEN, &open("~/My Repo")),
    )
    .await
    .expect("open answers")
    .expect("open succeeds");
    let canonical = cookie_agent_protocol::paths::canonicalize(&repository).unwrap();
    assert_eq!(
        opened.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert_eq!(opened.attachment_name, "labpc_my_repo");
    assert!(
        opened.tools.tools.iter().any(|tool| tool.name == "bash"),
        "{:?}",
        opened.tools.tools
    );

    let _: Empty = tokio::time::timeout(WAIT, peer.request(method::SHUTDOWN, &Empty {}))
        .await
        .expect("shutdown answers")
        .expect("shutdown succeeds");
    exits_successfully(&mut child).await;
}

/// A signal that asks the runtime to stop, as `kill`, systemd, or a closed
/// terminal sends, stops it serving and it exits cleanly.
#[cfg(unix)]
#[tokio::test]
async fn the_runtime_exits_cleanly_on_each_stop_signal() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let home = tempfile::tempdir().unwrap();
        let mut child = runtime(home.path(), &[]);
        let peer = connect(&mut child);
        initialize(&peer).await;
        let pid = i32::try_from(child.id().expect("runtime pid")).expect("pid fits");
        // SAFETY: plain signal delivery to a child process this test started.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "signal {pid}");
        exits_successfully(&mut child).await;
    }
}

#[tokio::test]
async fn the_runtime_refuses_workspaces_outside_its_roots_and_exits_on_stdin_eof() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut child = runtime(home.path(), &[&root.path().to_string_lossy()]);
    let peer = connect(&mut child);
    initialize(&peer).await;
    let error = tokio::time::timeout(
        WAIT,
        peer.request::<_, CapabilityOpenResult>(
            method::CAPABILITY_OPEN,
            &open(&home.path().to_string_lossy()),
        ),
    )
    .await
    .expect("open answers")
    .expect_err("outside the root");
    assert!(error.to_string().contains("outside"), "{error}");

    // Closing the daemon's end closes the runtime's standard input.
    peer.close();
    exits_successfully(&mut child).await;
}

#[tokio::test]
async fn a_missing_root_fails_on_stderr_with_nothing_on_stdout() {
    let home = tempfile::tempdir().unwrap();
    let child = runtime(home.path(), &["~/missing"]);
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("the runtime exits")
        .expect("output");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing"), "{stderr}");
    assert!(stderr.contains("unavailable"), "{stderr}");
}

/// The daemon token reaches cookie through `COOKIE_DAEMON_TOKEN` but goes no
/// further: a process the runtime starts, here the `bash` tool's shell, does
/// not inherit it, while the rest of the environment does.
#[cfg(unix)]
#[tokio::test]
async fn processes_the_runtime_starts_do_not_inherit_the_daemon_token() {
    use cookie_agent_protocol::{RunId, SessionId, ToolCallId};
    use cookie_agent_runtime_api::{
        TurnAgentContext,
        protocol::{
            ExecuteOutcome, ExecuteParams, ExecuteResult, Outcome, PrepareOutcome, PrepareParams,
            PrepareResult, ToolResult, ToolTurn, WireToolCall,
        },
        rpc::Incoming,
    };

    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cookie"))
        .args(["runtime", "serve", "--stdio"])
        .env("HOME", home.path())
        .env("COOKIE_DAEMON_TOKEN", "secret-daemon-token")
        .env("COOKIE_TEST_INHERITED", "inherited")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn cookie runtime");
    let transport = LineTransport::new(
        child.stdout.take().expect("stdout"),
        child.stdin.take().expect("stdin"),
    );
    let (peer, mut incoming) = Peer::start(Box::new(transport));
    // Acknowledge the bash call's progress, as a daemon does.
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Request(request) = message {
                let _ = request.respond(&Outcome::default()).await;
            }
        }
    });
    initialize(&peer).await;
    let opened: CapabilityOpenResult = tokio::time::timeout(
        WAIT,
        peer.request(
            method::CAPABILITY_OPEN,
            &open(&workspace.path().to_string_lossy()),
        ),
    )
    .await
    .expect("open answers")
    .expect("open succeeds");

    let turn = ToolTurn::from(&TurnAgentContext {
        agent: cookie_agent_protocol::AgentId::new("test").unwrap(),
        model: "test/model".parse().unwrap(),
        adapter: cookie_agent_protocol::AdaptorId::OPENAI_CHAT,
        input_media: Vec::new(),
    });
    let (session, run) = (SessionId::new_v7(), RunId::new_v7());
    let prepared: PrepareResult = tokio::time::timeout(
        WAIT,
        peer.request(
            method::TOOL_PREPARE,
            &PrepareParams {
                instance: Some(opened.instance),
                session,
                run,
                turn: turn.clone(),
                calls: vec![WireToolCall {
                    id: ToolCallId::new_v7(),
                    name: "bash".into(),
                    arguments: serde_json::json!({
                        "command": "printf '%s' \"${COOKIE_DAEMON_TOKEN-unset}\" > token.txt; \
                                    printf '%s' \"${COOKIE_TEST_INHERITED-unset}\" > inherited.txt"
                    }),
                }],
            },
        ),
    )
    .await
    .expect("prepare answers")
    .expect("prepare succeeds");
    let [PrepareOutcome::Prepared(bash)] = &prepared.results[..] else {
        panic!("unexpected outcomes {:?}", prepared.results);
    };
    let result: ExecuteResult = tokio::time::timeout(
        WAIT,
        peer.request(
            method::TOOL_EXECUTE,
            &ExecuteParams {
                background: false,
                prepared_id: bash.prepared_id,
                session,
                run,
                turn,
            },
        ),
    )
    .await
    .expect("execute answers")
    .expect("execute succeeds");
    assert!(
        matches!(
            result.outcome,
            ExecuteOutcome::Completed(ToolResult { failed: false, .. })
        ),
        "the bash call completes"
    );

    let read = |name: &str| std::fs::read_to_string(workspace.path().join(name)).unwrap();
    assert_eq!(read("inherited.txt"), "inherited");
    assert_eq!(read("token.txt"), "unset");

    let _: Empty = tokio::time::timeout(WAIT, peer.request(method::SHUTDOWN, &Empty {}))
        .await
        .expect("shutdown answers")
        .expect("shutdown succeeds");
    exits_successfully(&mut child).await;
}

/// `cookie runtime mcp-bridge`: what an ACP agent writes to its standard
/// input reaches the runtime's bridge socket, and what the socket sends
/// comes out of its standard output.
#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_bridge_copies_stdio_to_the_socket_and_back() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    let listener =
        cookie_agent_coding_runtime::acp_bridge::BridgeListener::bind().expect("a bridge socket");
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_cookie"))
        .args(["runtime", "mcp-bridge", "--socket", listener.path()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the bridge");
    let socket = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the bridge connects")
        .expect("a connection");
    let (socket_read, mut socket_write) = tokio::io::split(socket);
    let mut stdin = bridge.stdin.take().expect("stdin");
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut from_agent = tokio::io::BufReader::new(socket_read).lines();
    let line = tokio::time::timeout(WAIT, from_agent.next_line())
        .await
        .expect("the request reaches the socket")
        .unwrap()
        .unwrap();
    assert_eq!(line, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}");
    socket_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
        .await
        .unwrap();
    let mut to_agent = tokio::io::BufReader::new(bridge.stdout.take().expect("stdout")).lines();
    let line = tokio::time::timeout(WAIT, to_agent.next_line())
        .await
        .expect("the answer reaches the agent")
        .unwrap()
        .unwrap();
    assert_eq!(line, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}");
    drop(stdin);
    let status = tokio::time::timeout(WAIT, bridge.wait())
        .await
        .expect("the bridge exits when the agent closes its input")
        .unwrap();
    assert!(status.success(), "{status}");
}
