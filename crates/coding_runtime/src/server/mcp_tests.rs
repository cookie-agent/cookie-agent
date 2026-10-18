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
        McpChangedParams, McpReconnectParams, McpServerSettings, McpServerState, Outcome,
        PrepareOutcome, PrepareParams, PrepareResult, RuntimeSettings, RuntimeTools,
        ToolOutputLimits, ToolResult, ToolTurn, WireToolCall, method,
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
        let (session, run) = (SessionId::new_v7(), RunId::new_v7());
        let prepared: PrepareResult = self
            .daemon
            .request(
                method::TOOL_PREPARE,
                &PrepareParams {
                    instance: Some(self.instance()),
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
