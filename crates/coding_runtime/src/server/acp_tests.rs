//! The runtime's ACP agent relay (`docs/site/specs/acp-agents.md`
//! section 7): declarations, `agent/start` and `agent/stop`, the
//! `agent/message` relay, `agent/exited`, and the MCP bridge, against the
//! engine's scripted ACP agent.

use std::{path::Path, time::Duration};

use cookie_agent_protocol::{RUNTIME_PROTOCOL_VERSION, errors, in_process_pair};
use cookie_agent_runtime_api::{
    protocol::{
        AcpAgentReport, AcpAgentSettings, AgentExitedParams, AgentMcpOpenResult, AgentMcpParams,
        AgentMdLimits, AgentMessageParams, AgentRef, AgentsChangedParams, Empty, InitializeParams,
        InitializeResult, RuntimeSettings, ToolOutputLimits, method,
    },
    rpc::{Incoming, Peer},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{ServeOptions, serve};

const WAIT: Duration = Duration::from_secs(30);

#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "python";

fn fixture() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures/acp_agent.py")
        .to_string_lossy()
        .into_owned()
}

fn agent(id: &str, enabled: bool) -> AcpAgentSettings {
    AcpAgentSettings {
        id: id.to_owned(),
        command: vec![PYTHON.to_owned(), fixture()],
        env: Default::default(),
        description: format!("{id} agent"),
        enabled,
    }
}

fn initialize(agents: Vec<AcpAgentSettings>) -> InitializeParams {
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
            acp_agents: agents,
        },
    }
}

/// What the runtime sends the daemon besides answers.
enum Relayed {
    Message(AgentMessageParams),
    Exited(AgentExitedParams),
    Mcp(AgentMcpParams),
    Changed(AgentsChangedParams),
}

struct Harness {
    daemon: Peer,
    relayed: mpsc::UnboundedReceiver<Relayed>,
    _server: tokio::task::JoinHandle<String>,
}

fn start(options: ServeOptions) -> Harness {
    let (daemon_end, runtime_end) = in_process_pair(16);
    let server = tokio::spawn(serve(Box::new(runtime_end), options));
    let (daemon, mut incoming) = Peer::start(Box::new(daemon_end));
    let (sender, relayed) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            let Incoming::Notification(notification) = message else {
                continue;
            };
            let relayed = match notification.method.as_str() {
                method::AGENT_MESSAGE => Relayed::Message(notification.params().unwrap()),
                method::AGENT_EXITED => Relayed::Exited(notification.params().unwrap()),
                method::AGENT_MCP => Relayed::Mcp(notification.params().unwrap()),
                method::AGENTS_CHANGED => Relayed::Changed(notification.params().unwrap()),
                _ => continue,
            };
            let _ = sender.send(relayed);
        }
    });
    Harness {
        daemon,
        relayed,
        _server: server,
    }
}

impl Harness {
    async fn next(&mut self) -> Relayed {
        tokio::time::timeout(WAIT, self.relayed.recv())
            .await
            .expect("the runtime relays")
            .expect("the runtime is connected")
    }

    async fn next_message(&mut self) -> Value {
        loop {
            if let Relayed::Message(message) = self.next().await {
                return message.message;
            }
        }
    }

    async fn send(&self, agent: &str, message: Value) {
        self.daemon
            .notify(
                method::AGENT_MESSAGE,
                &AgentMessageParams {
                    agent: agent.to_owned(),
                    message,
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_agents_are_reported_and_relayed_until_they_exit() {
    let mut harness = start(ServeOptions::default());
    let result: InitializeResult = harness
        .daemon
        .request(
            method::INITIALIZE,
            &initialize(vec![agent("codex", true), agent("gemini", false)]),
        )
        .await
        .unwrap();
    assert_eq!(
        result.acp_agents,
        [
            AcpAgentReport {
                id: "codex".into(),
                description: "codex agent".into(),
                enabled: true,
            },
            AcpAgentReport {
                id: "gemini".into(),
                description: "gemini agent".into(),
                enabled: false,
            },
        ]
    );
    let _: Empty = harness
        .daemon
        .request(
            method::AGENT_START,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap();
    // Starting a running agent again is fine.
    let _: Empty = harness
        .daemon
        .request(
            method::AGENT_START,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap();
    harness
        .send(
            "codex",
            json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {"protocolVersion": 1}}),
        )
        .await;
    let answer = harness.next_message().await;
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["result"]["protocolVersion"], 1);
    assert_eq!(answer["result"]["agentInfo"]["name"], "fixture-agent");

    for (agent, detail) in [("gemini", "disabled"), ("claude", "hosts no ACP agent")] {
        let error = harness
            .daemon
            .request::<_, Empty>(
                method::AGENT_START,
                &AgentRef {
                    agent: agent.into(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.data_code(),
            Some(errors::AGENT_FAILED.code),
            "{agent}"
        );
        assert!(error.detail().unwrap().contains(detail), "{agent}: {error}");
    }

    harness
        .send(
            "codex",
            json!({"jsonrpc": "2.0", "id": 8, "method": "session/new", "params": {"cwd": "/", "mcpServers": []}}),
        )
        .await;
    let session = loop {
        let message = harness.next_message().await;
        if message["id"] == 8 {
            break message["result"]["sessionId"].as_str().unwrap().to_owned();
        }
    };
    harness
        .send(
            "codex",
            json!({"jsonrpc": "2.0", "id": 9, "method": "session/prompt",
                   "params": {"sessionId": session, "prompt": [{"type": "text", "text": "exit"}]}}),
        )
        .await;
    let exited = loop {
        if let Relayed::Exited(exited) = harness.next().await {
            break exited;
        }
    };
    assert_eq!(exited.agent, "codex");
    assert_eq!(exited.status, Some(3));
    assert!(
        exited.stderr.contains("fixture crashed on purpose"),
        "{exited:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_stop_kills_the_process_and_reports_it() {
    let mut harness = start(ServeOptions::default());
    let _: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize(vec![agent("codex", true)]))
        .await
        .unwrap();
    let _: Empty = harness
        .daemon
        .request(
            method::AGENT_START,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap();
    let _: Empty = harness
        .daemon
        .request(
            method::AGENT_STOP,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap();
    let exited = loop {
        if let Relayed::Exited(exited) = harness.next().await {
            break exited;
        }
    };
    assert_eq!(exited.message.as_deref(), Some("stopped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_settings_replace_the_agents_and_report_them() {
    let mut harness = start(ServeOptions::default());
    let result: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize(vec![agent("codex", true)]))
        .await
        .unwrap();
    assert_eq!(result.acp_agents.len(), 1);
    let settings = initialize(vec![agent("codex", true), agent("claude", true)]).settings;
    harness
        .daemon
        .notify(
            method::SETTINGS_CHANGED,
            &cookie_agent_runtime_api::protocol::RuntimeSettingsChangedParams { settings },
        )
        .await
        .unwrap();
    let changed = loop {
        if let Relayed::Changed(changed) = harness.next().await {
            break changed;
        }
    };
    let ids = changed
        .agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["claude", "codex"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_relays_each_connection_of_its_socket() {
    let mut harness = start(ServeOptions {
        bridge_command: vec!["bridge".to_owned()],
        ..ServeOptions::default()
    });
    let _: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize(vec![agent("codex", true)]))
        .await
        .unwrap();
    let refused = harness
        .daemon
        .request::<_, AgentMcpOpenResult>(
            method::AGENT_MCP_OPEN,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        refused.detail().unwrap().contains("not running"),
        "{refused}"
    );
    let _: Empty = harness
        .daemon
        .request(
            method::AGENT_START,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap();
    let bridge: AgentMcpOpenResult = harness
        .daemon
        .request(
            method::AGENT_MCP_OPEN,
            &AgentRef {
                agent: "codex".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(bridge.command[..2], ["bridge", "--socket"]);
    let path = std::path::PathBuf::from(&bridge.command[2]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "only the runtime's user enters {mode:o}");
    }
    let stream = crate::acp_bridge::connect(&path)
        .await
        .expect("the bridge socket");
    let (read, mut write) = tokio::io::split(stream);
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
        .await
        .unwrap();
    let relayed = loop {
        if let Relayed::Mcp(relayed) = harness.next().await {
            break relayed;
        }
    };
    assert_eq!(relayed.bridge, bridge.bridge);
    assert_eq!(relayed.message["method"], "tools/list");
    harness
        .daemon
        .notify(
            method::AGENT_MCP,
            &AgentMcpParams {
                bridge: bridge.bridge.clone(),
                connection: relayed.connection,
                message: json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}}),
            },
        )
        .await
        .unwrap();
    let mut lines = tokio::io::BufReader::new(read).lines();
    let line = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["tools"],
        json!([])
    );
    harness
        .daemon
        .notify(
            method::AGENT_MCP_CLOSE,
            &cookie_agent_runtime_api::protocol::AgentMcpBridgeRef {
                bridge: bridge.bridge.clone(),
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the socket is removed with its bridge");
}
