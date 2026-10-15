//! `cookie runtime coding --stdio`: the real binary serving the runtime
//! protocol over its standard input and output.

use std::{path::Path, process::Stdio, time::Duration};

use cookie_agent_runtime_api::{
    protocol::{
        AgentMdLimits, Empty, InitializeParams, InitializeResult, RUNTIME_PROTOCOL_VERSION,
        RuntimeSettings, ToolOutputLimits, method,
    },
    rpc::{LineTransport, Peer},
};
use tokio::process::{Child, Command};

const WAIT: Duration = Duration::from_secs(30);

fn runtime(home: &Path, workspace: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_cookie"))
        .args(["runtime", "coding", "--stdio", "--workspace", workspace])
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

fn params(workspace: Option<&str>) -> InitializeParams {
    InitializeParams {
        version: RUNTIME_PROTOCOL_VERSION,
        runtime_id: "labpc".into(),
        workspace: workspace.map(str::to_owned),
        settings: RuntimeSettings {
            tool_output: ToolOutputLimits {
                max_lines: 100,
                max_bytes: 10_000,
                head_lines: 50,
                tail_lines: 50,
            },
            agent_md: AgentMdLimits { max_bytes: 4096 },
            mcp_servers: Vec::new(),
        },
    }
}

async fn exits_successfully(child: &mut Child) {
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("the runtime exits")
        .expect("exit status");
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn the_runtime_expands_home_initializes_and_stops_on_shutdown() {
    let home = tempfile::tempdir().unwrap();
    let repository = home.path().join("My Repo");
    std::fs::create_dir(&repository).unwrap();
    let mut child = runtime(home.path(), "~/My Repo");
    let peer = connect(&mut child);

    // The daemon may name the workspace in its own spelling; the runtime
    // resolves it on its machine and reports the canonical path.
    let result: InitializeResult = tokio::time::timeout(
        WAIT,
        peer.request(method::INITIALIZE, &params(Some("~/My Repo"))),
    )
    .await
    .expect("initialize answers")
    .expect("initialize succeeds");
    let canonical = repository.canonicalize().unwrap();
    assert_eq!(
        result.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert_eq!(result.kind, cookie_agent_protocol::RuntimeKind::Coding);
    assert_eq!(result.attachment_name, "labpc_my_repo");
    assert!(
        result.tools.tools.iter().any(|tool| tool.name == "bash"),
        "{:?}",
        result.tools.tools
    );

    let _: Empty = tokio::time::timeout(WAIT, peer.request(method::SHUTDOWN, &Empty {}))
        .await
        .expect("shutdown answers")
        .expect("shutdown succeeds");
    exits_successfully(&mut child).await;
}

#[tokio::test]
async fn the_runtime_refuses_another_workspace_and_exits_on_stdin_eof() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut child = runtime(home.path(), &workspace.path().to_string_lossy());
    let peer = connect(&mut child);
    let error = tokio::time::timeout(
        WAIT,
        peer.request::<_, InitializeResult>(
            method::INITIALIZE,
            &params(Some(&home.path().to_string_lossy())),
        ),
    )
    .await
    .expect("initialize answers")
    .expect_err("a different workspace");
    assert!(error.to_string().contains("this runtime serves"), "{error}");

    // Closing the daemon's end closes the runtime's standard input.
    peer.close();
    exits_successfully(&mut child).await;
}

#[tokio::test]
async fn a_missing_workspace_fails_on_stderr_with_nothing_on_stdout() {
    let home = tempfile::tempdir().unwrap();
    let child = runtime(home.path(), "~/missing");
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
