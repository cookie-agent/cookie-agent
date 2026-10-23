//! A coding runtime hosting its template's MCP servers, driven through the
//! runtime protocol with the stdio fixture server of `cookie_agent_mcp`.

use std::{collections::BTreeMap, path::Path, time::Duration};

use base64::Engine as _;
use cookie_agent_protocol::{
    RUNTIME_PROTOCOL_VERSION, RunId, SessionId, ToolCallId, in_process_pair,
};
use cookie_agent_runtime_api::{
    protocol::{
        AgentMdLimits, CapabilityOpenParams, CapabilityOpenResult, CapabilityToolsChangedParams,
        ExecuteOutcome, ExecuteParams, ExecuteResult, InitializeParams, InitializeResult,
        McpChangedParams, McpReconnectParams, McpServerScope, McpServerSettings, McpServerState,
        Outcome, PrepareOutcome, PrepareParams, PrepareResult, RuntimeMcpChangedParams,
        RuntimeMcpReconnectParams, RuntimeSettings, RuntimeTools, ToolOutputLimits, ToolResult,
        ToolTurn, WireToolCall, method,
    },
    reconnect::ReconnectPolicy,
    rpc::{Incoming, Peer},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{ServeOptions, serve};

#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "python";

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../mcp/tests/fixtures/mcp_server.py"
);

/// Bound for waiting on the runtime; passing tests stop waiting at once.
const WAIT: Duration = Duration::from_secs(30);

/// The fixture server as a template lists it, with extra `env`.
fn fixture(env: &[(&str, &Path)]) -> Value {
    let env = env
        .iter()
        .map(|(name, path)| ((*name).to_owned(), path.to_string_lossy().into_owned()))
        .collect::<BTreeMap<_, _>>();
    json!({
        "command": PYTHON,
        "args": [FIXTURE],
        "env": env,
        "timeout_ms": 30_000,
    })
}

/// The initialize request for `servers` and the open request for
/// `workspace`.
fn initialize_params(
    workspace: &Path,
    servers: Vec<(&str, Value)>,
) -> (InitializeParams, CapabilityOpenParams) {
    let open = CapabilityOpenParams {
        capability: cookie_agent_protocol::CapabilityName::coding(),
        workspace: Some(workspace.to_string_lossy().into_owned()),
    };
    let initialize = InitializeParams {
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
            mcp_servers: servers
                .into_iter()
                .map(|(name, config)| McpServerSettings {
                    name: name.into(),
                    config,
                })
                .collect(),
        },
    };
    (initialize, open)
}

/// What the runtime tells the daemon on its own.
enum Notice {
    Tools(RuntimeTools),
    Mcp(McpChangedParams),
    /// `runtime/tools_changed`: the runtime's own tools.
    RuntimeTools(RuntimeTools),
    /// `runtime/mcp_changed`.
    RuntimeMcp(RuntimeMcpChangedParams),
}

/// A served runtime with a quick reconnect policy, and the daemon's peer.
struct Harness {
    daemon: Peer,
    notices: mpsc::UnboundedReceiver<Notice>,
    /// The instance [`Harness::initialize`] opened.
    instance: std::sync::OnceLock<String>,
}

fn start(reconnect: ReconnectPolicy) -> Harness {
    let (daemon_end, runtime_end) = in_process_pair(16);
    tokio::spawn(serve(
        Box::new(runtime_end),
        ServeOptions {
            mcp_reconnect: Some(reconnect),
            ..ServeOptions::default()
        },
    ));
    let (daemon, mut incoming) = Peer::start(Box::new(daemon_end));
    let (sender, notices) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Notification(notification) => {
                    let notice = match notification.method.as_str() {
                        method::CAPABILITY_TOOLS_CHANGED => Notice::Tools(
                            notification
                                .params::<CapabilityToolsChangedParams>()
                                .unwrap()
                                .tools,
                        ),
                        method::MCP_CHANGED => Notice::Mcp(notification.params().unwrap()),
                        method::TOOLS_CHANGED => {
                            Notice::RuntimeTools(notification.params().unwrap())
                        }
                        method::RUNTIME_MCP_CHANGED => {
                            Notice::RuntimeMcp(notification.params().unwrap())
                        }
                        _ => continue,
                    };
                    let _ = sender.send(notice);
                }
                Incoming::Request(request) => {
                    // Progress is accepted and dropped.
                    let _ = request.respond(&Outcome::default()).await;
                }
            }
        }
    });
    Harness {
        daemon,
        notices,
        instance: std::sync::OnceLock::new(),
    }
}

fn quick(attempts: usize) -> ReconnectPolicy {
    ReconnectPolicy::with_delays(vec![Duration::from_millis(20); attempts])
}

fn tool_names(tools: &RuntimeTools) -> Vec<&str> {
    tools.tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn turn(with_image: bool) -> ToolTurn {
    let mut turn = ToolTurn::from(crate::test_turn_context().as_ref());
    if with_image {
        let mut capabilities = crate::test_capabilities();
        capabilities
            .input
            .insert(cookie_agent_protocol::Modality::Image);
        capabilities.media = BTreeMap::from([(
            cookie_agent_protocol::MediaKind::Image,
            cookie_agent_protocol::MediaCapability {
                mime_types: [cookie_agent_protocol::MimeType::new("image/png").unwrap()]
                    .into_iter()
                    .collect(),
                max_bytes: 20 * 1024 * 1024,
                max_count: 1,
            },
        )]);
        turn.model.input_media = cookie_agent_runtime_api::media::tool_input_media(
            cookie_agent_runtime_api::AdapterFamily::OpenaiChat,
            &capabilities,
        );
    }
    turn
}

impl Harness {
    /// Initializes the runtime and opens `coding` for the workspace.
    async fn initialize(
        &self,
        (initialize, open): &(InitializeParams, CapabilityOpenParams),
    ) -> CapabilityOpenResult {
        let _: InitializeResult =
            tokio::time::timeout(WAIT, self.daemon.request(method::INITIALIZE, initialize))
                .await
                .expect("initialize answers")
                .expect("initialize succeeds");
        let opened: CapabilityOpenResult =
            tokio::time::timeout(WAIT, self.daemon.request(method::CAPABILITY_OPEN, open))
                .await
                .expect("open answers")
                .expect("open succeeds");
        let _ = self.instance.set(opened.instance.clone());
        opened
    }

    fn instance(&self) -> String {
        self.instance.get().expect("initialized").clone()
    }

    /// Prepares and executes one call.
    async fn call(&self, name: &str, arguments: Value, with_image: bool) -> ExecuteResult {
        self.call_on(Some(self.instance()), name, arguments, with_image)
            .await
    }

    /// Prepares and executes one call on `instance`, or on the runtime's own
    /// tools.
    async fn call_on(
        &self,
        instance: Option<String>,
        name: &str,
        arguments: Value,
        with_image: bool,
    ) -> ExecuteResult {
        let (session, run) = (SessionId::new_v7(), RunId::new_v7());
        let prepared: PrepareResult = self
            .daemon
            .request(
                method::TOOL_PREPARE,
                &PrepareParams {
                    instance,
                    session,
                    run,
                    turn: turn(with_image),
                    calls: vec![WireToolCall {
                        id: ToolCallId::new_v7(),
                        name: name.into(),
                        arguments,
                    }],
                },
            )
            .await
            .unwrap();
        let PrepareOutcome::Prepared(call) = prepared.results.into_iter().next().unwrap() else {
            panic!("`{name}` prepares");
        };
        assert_eq!(
            call.permission,
            Some(cookie_agent_protocol::PermissionAction::MCP)
        );
        assert_eq!(call.permission_resource.as_deref(), Some(name));
        tokio::time::timeout(
            WAIT,
            self.daemon.request(
                method::TOOL_EXECUTE,
                &ExecuteParams {
                    background: false,
                    prepared_id: call.prepared_id,
                    session,
                    run,
                    turn: turn(with_image),
                },
            ),
        )
        .await
        .expect("execute answers")
        .unwrap()
    }

    /// The next report whose server `fixture` is in `state`, skipping
    /// others; the tool lists sent meanwhile are collected.
    async fn wait_for_state(
        &mut self,
        state: McpServerState,
    ) -> (McpChangedParams, Vec<RuntimeTools>) {
        self.wait_for(state, false).await
    }

    /// [`Self::wait_for_state`] that, with `failure`, also waits for a
    /// failure report, which may arrive in a report of its own.
    async fn wait_for(
        &mut self,
        state: McpServerState,
        failure: bool,
    ) -> (McpChangedParams, Vec<RuntimeTools>) {
        let mut tools = Vec::new();
        let mut failures = Vec::new();
        loop {
            let notice = tokio::time::timeout(WAIT, self.notices.recv())
                .await
                .unwrap_or_else(|_| panic!("the fixture never became {state:?}"))
                .expect("runtime running");
            match notice {
                Notice::Tools(list) => tools.push(list),
                Notice::Mcp(mut changed) => {
                    // Every report carries every server's status.
                    failures.append(&mut changed.failures);
                    if changed.servers[0].state == state && (!failure || !failures.is_empty()) {
                        changed.failures = failures;
                        return (changed, tools);
                    }
                }
                Notice::RuntimeTools(_) | Notice::RuntimeMcp(_) => {}
            }
        }
    }
}

#[tokio::test]
async fn template_servers_publish_their_tools_and_answer_calls_with_media() {
    let workspace = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("image");
    let harness = start(quick(3));
    let result = harness
        .initialize(&initialize_params(
            workspace.path(),
            vec![("fixture", fixture(&[("MCP_FIXTURE_IMAGE", &marker)]))],
        ))
        .await;
    assert_eq!(
        tool_names(&result.tools),
        [
            "read",
            "write",
            "edit",
            "bash",
            "webfetch",
            "fixture_echo_text",
            "fixture_fail",
            "fixture_image",
        ]
    );
    let echo = &result.tools.tools[5];
    assert_eq!(
        echo.permission,
        Some(cookie_agent_protocol::PermissionAction::MCP)
    );
    assert!(
        echo.description.contains("untrusted MCP output"),
        "{}",
        echo.description
    );
    assert_eq!(result.mcp_servers.len(), 1);
    let server = &result.mcp_servers[0];
    assert_eq!(server.name, "fixture");
    assert_eq!(server.state, McpServerState::Connected);
    assert_eq!(
        server.tools,
        ["fixture_echo_text", "fixture_fail", "fixture_image"]
    );

    let echoed = harness
        .call("fixture_echo_text", json!({"text": "hello"}), false)
        .await;
    let ExecuteOutcome::Completed(ToolResult { output, failed, .. }) = echoed.outcome else {
        panic!("the call completes");
    };
    assert!(!failed);
    assert_eq!(
        output,
        cookie_agent_protocol::ToolCompletionOutput::Single {
            text: "hello".into()
        }
    );
    let failing = harness.call("fixture_fail", json!({}), false).await;
    assert!(matches!(
        failing.outcome,
        ExecuteOutcome::Completed(ToolResult { failed: true, .. })
    ));

    let image = harness.call("fixture_image", json!({}), true).await;
    let ExecuteOutcome::Completed(result) = image.outcome else {
        panic!("the image call completes");
    };
    assert!(!result.failed);
    let attachment = result
        .all_attachments()
        .next()
        .expect("the image is an attachment");
    assert_eq!(attachment.mime_type.as_str(), "image/png");
    assert_eq!(image.media.len(), 1);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.media[0].data)
        .unwrap();
    assert_eq!(image.media[0].sha256, attachment.sha256.as_str());
    assert_eq!(
        cookie_agent_protocol::Sha256Digest::of_bytes(&bytes).as_str(),
        image.media[0].sha256
    );
}

#[tokio::test]
async fn a_crashed_server_reconnects_and_one_that_cannot_is_dropped_and_reported() {
    let workspace = tempfile::tempdir().unwrap();
    let refuse = workspace.path().join("refuse");
    let mut harness = start(ReconnectPolicy::with_delays(vec![
        Duration::from_millis(300),
        Duration::from_millis(20),
    ]));
    harness
        .initialize(&initialize_params(
            workspace.path(),
            vec![("fixture", fixture(&[("MCP_FIXTURE_REFUSE_FILE", &refuse)]))],
        ))
        .await;

    // The server exits in the middle of a call: the call fails as
    // unavailable, the tools stay declared while it reconnects, and the
    // restarted server is connected again.
    let crashed = harness
        .call("fixture_echo_text", json!({"text": "exit"}), false)
        .await;
    let ExecuteOutcome::Failed { error } = crashed.outcome else {
        panic!("the crashed call fails");
    };
    assert_eq!(
        error.kind,
        cookie_agent_runtime_api::protocol::ToolErrorKind::RuntimeUnavailable
    );
    let (reconnecting, tools) = harness.wait_for_state(McpServerState::Reconnecting).await;
    assert!(tools.is_empty(), "the tool list stays while reconnecting");
    assert_eq!(
        reconnecting.servers[0].tools,
        ["fixture_echo_text", "fixture_fail"]
    );
    let refused = harness
        .call("fixture_echo_text", json!({"text": "hello"}), false)
        .await;
    assert!(matches!(
        refused.outcome,
        ExecuteOutcome::Failed { error } if error.kind
            == cookie_agent_runtime_api::protocol::ToolErrorKind::RuntimeUnavailable
    ));
    let (connected, tools) = harness.wait_for_state(McpServerState::Connected).await;
    assert!(connected.failures.is_empty());
    assert!(tools.is_empty(), "a restored server keeps its tool list");
    let echoed = harness
        .call("fixture_echo_text", json!({"text": "back"}), false)
        .await;
    assert!(matches!(
        echoed.outcome,
        ExecuteOutcome::Completed(ToolResult { failed: false, .. })
    ));

    // When every restart fails, its tools leave the list and the daemon is
    // told why.
    std::fs::write(&refuse, "").unwrap();
    let crashed = harness
        .call("fixture_echo_text", json!({"text": "exit"}), false)
        .await;
    assert!(matches!(crashed.outcome, ExecuteOutcome::Failed { .. }));
    let (failed, tools) = harness.wait_for(McpServerState::Failed, true).await;
    assert_eq!(failed.failures.len(), 1, "{:?}", failed.failures);
    assert_eq!(failed.failures[0].server, "fixture");
    assert!(
        failed.failures[0]
            .reason
            .contains("did not reconnect after 2 attempts"),
        "{}",
        failed.failures[0].reason
    );
    assert_eq!(
        tool_names(tools.last().expect("a new tool list")),
        ["read", "write", "edit", "bash", "webfetch"]
    );
    let gone: PrepareResult = harness
        .daemon
        .request(
            method::TOOL_PREPARE,
            &PrepareParams {
                instance: Some(harness.instance()),
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                turn: turn(false),
                calls: vec![WireToolCall {
                    id: ToolCallId::new_v7(),
                    name: "fixture_echo_text".into(),
                    arguments: json!({"text": "hello"}),
                }],
            },
        )
        .await
        .unwrap();
    assert!(matches!(gone.results[0], PrepareOutcome::Failed { .. }));
}

#[tokio::test]
async fn a_server_that_fails_at_initialize_publishes_nothing_until_it_reconnects() {
    let workspace = tempfile::tempdir().unwrap();
    let refuse = workspace.path().join("refuse");
    std::fs::write(&refuse, "").unwrap();
    let mut harness = start(quick(1));
    let result = harness
        .initialize(&initialize_params(
            workspace.path(),
            vec![
                ("fixture", fixture(&[("MCP_FIXTURE_REFUSE_FILE", &refuse)])),
                (
                    "missing",
                    json!({"command": "cookie-agent-test-no-such-mcp-server"}),
                ),
            ],
        ))
        .await;
    assert_eq!(
        tool_names(&result.tools),
        ["read", "write", "edit", "bash", "webfetch"]
    );
    let states = result
        .mcp_servers
        .iter()
        .map(|server| (server.name.as_str(), server.state))
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [
            ("fixture", McpServerState::Failed),
            ("missing", McpServerState::Failed)
        ]
    );
    assert!(
        result.mcp_servers[1]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("cookie-agent-test-no-such-mcp-server")),
        "{:?}",
        result.mcp_servers[1].message
    );

    // A retry of every failed server answers once the attempts finished.
    std::fs::remove_file(&refuse).unwrap();
    let retried: Outcome = tokio::time::timeout(
        WAIT,
        harness.daemon.request(
            method::MCP_RECONNECT,
            &McpReconnectParams {
                instance: harness.instance(),
                server: None,
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(retried, Outcome::default());
    let (connected, tools) = harness.wait_for_state(McpServerState::Connected).await;
    assert_eq!(connected.servers[1].state, McpServerState::Failed);
    assert_eq!(
        tool_names(tools.last().expect("a new tool list")),
        [
            "read",
            "write",
            "edit",
            "bash",
            "webfetch",
            "fixture_echo_text",
            "fixture_fail"
        ]
    );

    // Reconnecting one server by name reports its failure.
    let outcome: Outcome = harness
        .daemon
        .request(
            method::MCP_RECONNECT,
            &McpReconnectParams {
                instance: harness.instance(),
                server: Some("missing".into()),
            },
        )
        .await
        .unwrap();
    assert!(outcome.error.is_some());
    let outcome: Outcome = harness
        .daemon
        .request(
            method::MCP_RECONNECT,
            &McpReconnectParams {
                instance: harness.instance(),
                server: Some("unknown".into()),
            },
        )
        .await
        .unwrap();
    assert!(
        outcome
            .error
            .is_some_and(|error| error.message.contains("unknown MCP server"))
    );
}

#[tokio::test]
async fn template_servers_that_enable_oauth_or_do_not_parse_are_refused() {
    let workspace = tempfile::tempdir().unwrap();
    for config in [
        json!({"url": "https://mcp.example.test/mcp", "oauth": true}),
        json!({"url": "https://mcp.example.test/mcp", "oauth": {"client_id": "cookie"}}),
        json!({"command": "docs-mcp", "lazy": true}),
        json!({"command": "docs-mcp", "url": "https://mcp.example.test/mcp"}),
    ] {
        let harness = start(quick(1));
        let error = harness
            .daemon
            .request::<_, InitializeResult>(
                method::INITIALIZE,
                &initialize_params(workspace.path(), vec![("docs", config.clone())]).0,
            )
            .await
            .expect_err("refused");
        let message = error.to_string();
        assert_eq!(
            error.data_code(),
            Some("initialize_rejected"),
            "{config}: {message}"
        );
        assert!(message.contains("`docs`"), "{config}: {message}");
    }
}

/// `config` with `scope`.
fn scoped(mut config: Value, scope: &str) -> Value {
    config["scope"] = json!(scope);
    config
}

/// The fixture writing its working directory to `cwd_file`, started in
/// `cwd` when one is given.
fn reporting(cwd_file: &Path, cwd: Option<&str>) -> Value {
    let mut config = fixture(&[("MCP_FIXTURE_CWD_FILE", cwd_file)]);
    if let Some(cwd) = cwd {
        config["cwd"] = json!(cwd);
    }
    config
}

/// The canonical directory the fixture wrote to `cwd_file`.
fn reported_cwd(cwd_file: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(std::fs::read_to_string(cwd_file).expect("the fixture's cwd"))
        .canonicalize()
        .expect("canonical cwd")
}

fn open_params(workspace: &Path) -> CapabilityOpenParams {
    CapabilityOpenParams {
        capability: cookie_agent_protocol::CapabilityName::coding(),
        workspace: Some(workspace.to_string_lossy().into_owned()),
    }
}

impl Harness {
    async fn initialize_only(&self, initialize: &InitializeParams) -> InitializeResult {
        tokio::time::timeout(WAIT, self.daemon.request(method::INITIALIZE, initialize))
            .await
            .expect("initialize answers")
            .expect("initialize succeeds")
    }

    async fn open(&self, workspace: &Path) -> CapabilityOpenResult {
        tokio::time::timeout(
            WAIT,
            self.daemon
                .request(method::CAPABILITY_OPEN, &open_params(workspace)),
        )
        .await
        .expect("open answers")
        .expect("open succeeds")
    }

    /// The next `runtime/mcp_changed` whose server `name` is in `state`
    /// (and, with `failure`, after a failure report); the runtime's tool
    /// lists sent meanwhile are collected.
    async fn wait_runtime(
        &mut self,
        name: &str,
        state: McpServerState,
        failure: bool,
    ) -> (RuntimeMcpChangedParams, Vec<RuntimeTools>) {
        let mut tools = Vec::new();
        let mut failures = Vec::new();
        loop {
            let notice = tokio::time::timeout(WAIT, self.notices.recv())
                .await
                .unwrap_or_else(|_| panic!("`{name}` never became {state:?}"))
                .expect("runtime running");
            match notice {
                Notice::RuntimeTools(list) => tools.push(list),
                Notice::RuntimeMcp(mut changed) => {
                    failures.append(&mut changed.failures);
                    let reached = changed
                        .servers
                        .iter()
                        .any(|server| server.name == name && server.state == state);
                    if reached && (!failure || !failures.is_empty()) {
                        changed.failures = failures;
                        return (changed, tools);
                    }
                }
                Notice::Tools(_) | Notice::Mcp(_) => {}
            }
        }
    }
}

fn report_summary(
    servers: &[cookie_agent_runtime_api::protocol::McpServerReport],
) -> Vec<(&str, Option<McpServerScope>, McpServerState, usize)> {
    servers
        .iter()
        .map(|server| {
            (
                server.name.as_str(),
                server.scope,
                server.state,
                server.tools.len(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_runtime_scoped_server_starts_with_the_runtime_and_every_workspace_shares_it() {
    let scratch = tempfile::tempdir().unwrap();
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for workspace in [&first, &second] {
        std::fs::create_dir(workspace.path().join("sub")).unwrap();
    }
    let pid_file = scratch.path().join("browser.pid");
    let browser_cwd = scratch.path().join("browser.cwd");
    let docs_cwd = scratch.path().join("docs.cwd");
    let nested_cwd = scratch.path().join("nested.cwd");
    let mut browser = reporting(&browser_cwd, None);
    browser["env"]["MCP_FIXTURE_PID_FILE"] = json!(pid_file.to_string_lossy());
    let (initialize, _) = initialize_params(
        first.path(),
        vec![
            ("browser", scoped(browser, "runtime")),
            ("docs", reporting(&docs_cwd, None)),
            (
                "nested",
                scoped(reporting(&nested_cwd, Some("sub")), "workspace"),
            ),
        ],
    );
    let mut harness = start(quick(1));

    // The reply lists every configured server: the runtime-scoped one on
    // its way, the workspace-scoped ones as configured.
    let initialized = harness.initialize_only(&initialize).await;
    assert_eq!(
        report_summary(&initialized.mcp_servers),
        [
            (
                "browser",
                Some(McpServerScope::Runtime),
                McpServerState::Connecting,
                0
            ),
            (
                "docs",
                Some(McpServerScope::Workspace),
                McpServerState::Disconnected,
                0
            ),
            (
                "nested",
                Some(McpServerScope::Workspace),
                McpServerState::Disconnected,
                0
            ),
        ]
    );

    // It connects with no workspace open, in the runtime's home, and its
    // tools are the runtime's own.
    let (connected, tools) = harness
        .wait_runtime("browser", McpServerState::Connected, false)
        .await;
    assert_eq!(
        report_summary(&connected.servers)[0],
        (
            "browser",
            Some(McpServerScope::Runtime),
            McpServerState::Connected,
            2
        )
    );
    assert_eq!(
        tool_names(tools.last().expect("the runtime's tools")),
        ["browser_echo_text", "browser_fail"]
    );
    assert_eq!(
        reported_cwd(&browser_cwd),
        crate::runtime_home()
            .expect("a home directory")
            .canonicalize()
            .expect("canonical home")
    );
    let pid = std::fs::read_to_string(&pid_file).expect("the browser started");

    // Each workspace hosts its own workspace-scoped servers, in the
    // workspace (a relative `cwd` under it), and declares none of the
    // runtime's.
    for workspace in [&first, &second] {
        let opened = harness.open(workspace.path()).await;
        assert_eq!(
            tool_names(&opened.tools),
            [
                "read",
                "write",
                "edit",
                "bash",
                "webfetch",
                "docs_echo_text",
                "docs_fail",
                "nested_echo_text",
                "nested_fail",
            ]
        );
        assert_eq!(
            report_summary(&opened.mcp_servers),
            [
                (
                    "docs",
                    Some(McpServerScope::Workspace),
                    McpServerState::Connected,
                    2
                ),
                (
                    "nested",
                    Some(McpServerScope::Workspace),
                    McpServerState::Connected,
                    2
                ),
            ]
        );
        let root = workspace.path().canonicalize().unwrap();
        assert_eq!(reported_cwd(&docs_cwd), root);
        assert_eq!(reported_cwd(&nested_cwd), root.join("sub"));
    }
    // Still the one browser process.
    assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), pid);

    // Its tools are called without an instance; an instance's are not
    // reached that way.
    let echoed = harness
        .call_on(None, "browser_echo_text", json!({"text": "shared"}), false)
        .await;
    let ExecuteOutcome::Completed(ToolResult { output, failed, .. }) = echoed.outcome else {
        panic!("the call completes");
    };
    assert!(!failed);
    assert_eq!(
        output,
        cookie_agent_protocol::ToolCompletionOutput::Single {
            text: "shared".into()
        }
    );
    let missing: PrepareResult = harness
        .daemon
        .request(
            method::TOOL_PREPARE,
            &PrepareParams {
                instance: None,
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                turn: turn(false),
                calls: vec![WireToolCall {
                    id: ToolCallId::new_v7(),
                    name: "docs_echo_text".into(),
                    arguments: json!({"text": "hello"}),
                }],
            },
        )
        .await
        .unwrap();
    assert!(matches!(missing.results[0], PrepareOutcome::Failed { .. }));
}

#[tokio::test]
async fn a_runtime_scoped_server_reconnects_and_reports_when_it_cannot() {
    let scratch = tempfile::tempdir().unwrap();
    let refuse = scratch.path().join("refuse");
    let workspace = tempfile::tempdir().unwrap();
    let (initialize, _) = initialize_params(
        workspace.path(),
        vec![(
            "browser",
            scoped(fixture(&[("MCP_FIXTURE_REFUSE_FILE", &refuse)]), "runtime"),
        )],
    );
    let mut harness = start(ReconnectPolicy::with_delays(vec![
        Duration::from_millis(300),
        Duration::from_millis(20),
    ]));
    harness.initialize_only(&initialize).await;
    // An open waits for its first attempt, whose report reaches the daemon
    // first.
    harness.open(workspace.path()).await;
    let (_, tools) = harness
        .wait_runtime("browser", McpServerState::Connected, false)
        .await;
    assert_eq!(
        tool_names(tools.last().expect("the runtime's tools")),
        ["browser_echo_text", "browser_fail"]
    );

    // The server exits mid-call: it reconnects, its tools staying declared.
    let crashed = harness
        .call_on(None, "browser_echo_text", json!({"text": "exit"}), false)
        .await;
    assert!(matches!(crashed.outcome, ExecuteOutcome::Failed { .. }));
    let (_, tools) = harness
        .wait_runtime("browser", McpServerState::Reconnecting, false)
        .await;
    assert!(tools.is_empty(), "the tool list stays while reconnecting");
    let (connected, _) = harness
        .wait_runtime("browser", McpServerState::Connected, false)
        .await;
    assert!(connected.failures.is_empty());

    // When every restart fails, its tools leave the runtime's list and the
    // daemon hears why.
    std::fs::write(&refuse, "").unwrap();
    let crashed = harness
        .call_on(None, "browser_echo_text", json!({"text": "exit"}), false)
        .await;
    assert!(matches!(crashed.outcome, ExecuteOutcome::Failed { .. }));
    let (failed, tools) = harness
        .wait_runtime("browser", McpServerState::Failed, true)
        .await;
    assert_eq!(failed.failures.len(), 1, "{:?}", failed.failures);
    assert_eq!(failed.failures[0].server, "browser");
    assert!(tool_names(tools.last().expect("a new tool list")).is_empty());

    // A retry of the failed ones brings it back.
    std::fs::remove_file(&refuse).unwrap();
    let retried: Outcome = tokio::time::timeout(
        WAIT,
        harness.daemon.request(
            method::RUNTIME_MCP_RECONNECT,
            &RuntimeMcpReconnectParams { server: None },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(retried, Outcome::default());
    let (_, tools) = harness
        .wait_runtime("browser", McpServerState::Connected, false)
        .await;
    assert_eq!(
        tool_names(tools.last().expect("the tools are back")),
        ["browser_echo_text", "browser_fail"]
    );
    let unknown: Outcome = harness
        .daemon
        .request(
            method::RUNTIME_MCP_RECONNECT,
            &RuntimeMcpReconnectParams {
                server: Some("docs".into()),
            },
        )
        .await
        .unwrap();
    assert!(unknown.error.is_some());
}

#[tokio::test]
async fn a_daemon_before_runtime_scoped_servers_gets_them_in_every_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let (mut initialize, open) = initialize_params(
        workspace.path(),
        vec![("browser", scoped(fixture(&[]), "runtime"))],
    );
    initialize.protocol_version = cookie_agent_protocol::SemVer::new(1, 4, 0);
    let harness = start(quick(1));
    let initialized = harness.initialize_only(&initialize).await;
    assert_eq!(
        report_summary(&initialized.mcp_servers),
        [(
            "browser",
            Some(McpServerScope::Workspace),
            McpServerState::Disconnected,
            0
        )]
    );
    let opened: CapabilityOpenResult = harness
        .daemon
        .request(method::CAPABILITY_OPEN, &open)
        .await
        .unwrap();
    assert!(
        tool_names(&opened.tools).contains(&"browser_echo_text"),
        "{:?}",
        tool_names(&opened.tools)
    );
}
