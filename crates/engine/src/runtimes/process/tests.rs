//! Runtime processes started from a command: the environment, stderr
//! diagnostics, the startup timeout, and process cleanup.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use cookie_agent_runtime_api::{
    DiagnosticTail,
    protocol::{AgentMdLimits, RuntimeSettings, ToolOutputLimits},
};

use super::ProcessConnector;
use crate::runtimes::{ProtocolTemplate, RuntimeTemplate};

fn settings() -> RuntimeSettings {
    RuntimeSettings {
        shell_environment: None,
        tool_output: ToolOutputLimits {
            max_lines: 100,
            max_bytes: 10_000,
            head_lines: 50,
            tail_lines: 50,
        },
        agent_md: AgentMdLimits { max_bytes: 4096 },
        mcp_servers: Vec::new(),
        acp_agents: Vec::new(),
    }
}

fn template(script: &str, env: &[(&str, &str)], startup_timeout: Duration) -> ProtocolTemplate {
    let command = ["sh", "-c", script, "runtime"].map(str::to_owned).to_vec();
    let env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    ProtocolTemplate::new(
        "labpc",
        Arc::new(ProcessConnector::new(
            command,
            env,
            Duration::from_millis(200),
        )),
        settings(),
        startup_timeout,
        Duration::from_millis(200),
    )
}

async fn start_error(template: &ProtocolTemplate, workspace: &str) -> String {
    template
        .start(
            &cookie_agent_protocol::CapabilityName::coding(),
            Some(Path::new(workspace)),
        )
        .await
        .err()
        .expect("the runtime does not start")
        .to_string()
}

#[tokio::test]
async fn env_adds_to_the_environment() {
    let template = template(
        r#"printf '%s|%s\n' "$RUNTIME_MODE" "${PATH:+inherited}" >&2; exit 3"#,
        &[("RUNTIME_MODE", "remote")],
        Duration::from_secs(20),
    );
    let error = start_error(&template, "/srv/api").await;
    assert!(error.contains("exit status: 3"), "{error}");
    assert!(error.ends_with("stderr:\nremote|inherited"), "{error}");
}

#[tokio::test]
async fn stderr_reaches_the_failure_reason_bounded_to_its_last_lines() {
    let template = template(
        "i=0; while [ $i -lt 100 ]; do echo \"line $i\" >&2; i=$((i+1)); done; exit 255",
        &[],
        Duration::from_secs(20),
    );
    let error = start_error(&template, "/srv/api").await;
    let (_, stderr) = error.split_once("stderr:\n").expect("stderr tail");
    assert_eq!(stderr.lines().count(), DiagnosticTail::MAX_LINES);
    assert!(stderr.ends_with("line 99"), "{stderr}");
    assert!(!stderr.contains("line 67\n"), "{stderr}");
}

#[tokio::test]
async fn a_runtime_that_does_not_initialize_in_time_is_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let template = template(
        &format!(
            "echo $$ > '{}'; echo 'waiting for a password' >&2; sleep 30",
            pid_file.display()
        ),
        &[],
        Duration::from_millis(500),
    );
    let started = std::time::Instant::now();
    let error = start_error(&template, "/srv/api").await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(error.contains("did not start within 500 ms"), "{error}");
    assert!(
        error.ends_with("stderr:\nwaiting for a password"),
        "{error}"
    );

    // The process group is killed once the shutdown grace passes.
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "process {pid} still runs"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_missing_program_fails_to_start() {
    let template = ProtocolTemplate::new(
        "labpc",
        Arc::new(ProcessConnector::new(
            vec!["/nonexistent/cookie-runtime".into()],
            BTreeMap::new(),
            Duration::from_millis(200),
        )),
        settings(),
        Duration::from_secs(20),
        Duration::from_millis(200),
    );
    let error = template.capabilities().await.expect_err("no such program");
    assert!(
        error.contains("failed to run `/nonexistent/cookie-runtime`"),
        "{error}"
    );
}
