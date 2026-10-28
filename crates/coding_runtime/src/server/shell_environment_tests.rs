//! The user's shell environment over the runtime protocol (shell
//! environment spec sections 5 to 9): captured at `runtime/initialize`,
//! reported, used by the `bash` tool, captured again on
//! `runtime/shell_environment_refresh`, and dropped when settings turn it
//! off.

use std::time::Duration;

use cookie_agent_protocol::{
    RUNTIME_PROTOCOL_VERSION, RunId, SessionId, ToolCallId, in_process_pair,
};
use cookie_agent_runtime_api::{
    protocol::{
        AgentMdLimits, CapabilityOpenParams, CapabilityOpenResult, Empty, ExecuteParams,
        ExecuteResult, InitializeParams, InitializeResult, Outcome, PrepareOutcome, PrepareParams,
        PrepareResult, RuntimeSettings, RuntimeSettingsChangedParams, ShellCapture,
        ShellEnvironmentReport, ShellEnvironmentSettings, ToolOutputLimits, ToolProgressParams,
        ToolTurn, WireToolCall, method,
    },
    rpc::{Incoming, Peer},
};
use serde_json::json;
use tokio::sync::mpsc;

use super::{ServeOptions, serve};

const WAIT: Duration = Duration::from_secs(20);

/// A shell whose "rc file" exports `FROM_THE_SHELL` from `value`: it drops
/// the login and interactive flags and runs the capture script with sh.
fn fake_shell(directory: &std::path::Path, value: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let shell = directory.join("fake-shell");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\nFROM_THE_SHELL=\"$(cat '{}')\"\nexport FROM_THE_SHELL\n\
             while [ \"$1\" = -l ] || [ \"$1\" = -i ]; do shift; done\nexec /bin/sh \"$@\"\n",
            value.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    shell
}

fn settings(shell_environment: ShellEnvironmentSettings) -> RuntimeSettings {
    RuntimeSettings {
        tool_output: ToolOutputLimits {
            max_lines: 100,
            max_bytes: 10_000,
            head_lines: 50,
            tail_lines: 50,
        },
        agent_md: AgentMdLimits { max_bytes: 4096 },
        mcp_servers: Vec::new(),
        acp_agents: Vec::new(),
        shell_environment: Some(shell_environment),
    }
}

struct Harness {
    daemon: Peer,
    reports: mpsc::UnboundedReceiver<ShellEnvironmentReport>,
    progress: mpsc::UnboundedReceiver<ToolProgressParams>,
}

fn start() -> Harness {
    let (daemon_end, runtime_end) = in_process_pair(16);
    tokio::spawn(serve(Box::new(runtime_end), ServeOptions::default()));
    let (daemon, mut incoming) = Peer::start(Box::new(daemon_end));
    let (report_sender, reports) = mpsc::unbounded_channel();
    let (progress_sender, progress) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Notification(notification)
                    if notification.method == method::SHELL_ENVIRONMENT =>
                {
                    let _ = report_sender.send(notification.params().unwrap());
                }
                Incoming::Request(request) if request.method == method::TOOL_PROGRESS => {
                    let _ = progress_sender.send(request.params().unwrap());
                    request.respond(&Outcome::default()).await.unwrap();
                }
                _ => {}
            }
        }
    });
    Harness {
        daemon,
        reports,
        progress,
    }
}

impl Harness {
    async fn report(&mut self) -> ShellEnvironmentReport {
        tokio::time::timeout(WAIT, self.reports.recv())
            .await
            .expect("a report in time")
            .expect("reports open")
    }

    /// What a `bash` call printing `FROM_THE_SHELL` writes.
    async fn bash_sees(&mut self, instance: &str) -> String {
        let session = SessionId::new_v7();
        let run = RunId::new_v7();
        let turn = ToolTurn::from(crate::test_turn_context().as_ref());
        let prepared: PrepareResult = self
            .daemon
            .request(
                method::TOOL_PREPARE,
                &PrepareParams {
                    instance: Some(instance.to_owned()),
                    session,
                    run,
                    turn: turn.clone(),
                    calls: vec![WireToolCall {
                        id: ToolCallId::new_v7(),
                        name: "bash".into(),
                        arguments: json!({"command": "printf '[%s]' \"$FROM_THE_SHELL\""}),
                    }],
                },
            )
            .await
            .unwrap();
        let [PrepareOutcome::Prepared(bash)] = &prepared.results[..] else {
            panic!("unexpected outcomes {:?}", prepared.results);
        };
        let _: ExecuteResult = self
            .daemon
            .request(
                method::TOOL_EXECUTE,
                &ExecuteParams {
                    background: false,
                    prepared_id: bash.prepared_id,
                    session,
                    run,
                    turn,
                },
            )
            .await
            .unwrap();
        let mut streamed = String::new();
        while let Ok(progress) = self.progress.try_recv() {
            for chunk in progress.progress.output {
                streamed.push_str(&chunk.text);
            }
        }
        streamed
    }
}

#[tokio::test]
async fn the_runtime_captures_reports_refreshes_and_drops_the_shell_environment() {
    let directory = tempfile::tempdir().unwrap();
    let value = directory.path().join("value");
    std::fs::write(&value, "first").unwrap();
    let shell = fake_shell(directory.path(), &value);
    let capture = ShellEnvironmentSettings {
        capture: ShellCapture::Interactive,
        timeout_ms: 20_000,
        shell: shell.to_string_lossy().into_owned(),
    };
    let mut harness = start();
    let _: InitializeResult = harness
        .daemon
        .request(
            method::INITIALIZE,
            &InitializeParams {
                protocol_version: RUNTIME_PROTOCOL_VERSION,
                engine_version: "test".into(),
                features: Vec::new(),
                runtime_id: "labpc".into(),
                settings: settings(capture.clone()),
            },
        )
        .await
        .unwrap();
    let report = harness.report().await;
    assert!(report.captured, "{report:?}");
    assert_eq!(report.shell, capture.shell);
    assert!(report.variables > 0);
    assert!(!report.snapshot, "only bash gets a snapshot");
    assert_eq!(report.error, None);

    let opened: CapabilityOpenResult = harness
        .daemon
        .request(
            method::CAPABILITY_OPEN,
            &CapabilityOpenParams {
                capability: cookie_agent_protocol::CapabilityName::coding(),
                workspace: Some(directory.path().to_string_lossy().into_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(harness.bash_sees(&opened.instance).await, "[first]");

    // An edited rc file is picked up on a refresh.
    std::fs::write(&value, "second").unwrap();
    harness
        .daemon
        .notify(method::SHELL_ENVIRONMENT_REFRESH, &Empty {})
        .await
        .unwrap();
    assert!(harness.report().await.captured);
    assert_eq!(harness.bash_sees(&opened.instance).await, "[second]");

    // Off: the runtime's own environment, which has no such variable.
    harness
        .daemon
        .notify(
            method::SETTINGS_CHANGED,
            &RuntimeSettingsChangedParams {
                settings: settings(ShellEnvironmentSettings {
                    capture: ShellCapture::Off,
                    ..capture
                }),
            },
        )
        .await
        .unwrap();
    let report = harness.report().await;
    assert!(!report.captured);
    assert_eq!(report.error, None);
    assert_eq!(harness.bash_sees(&opened.instance).await, "[]");
}
