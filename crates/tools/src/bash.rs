use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolInvocationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::OutputStream;
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

use crate::{RESULT_LIMIT, result, schema, tool_error, workspace_for, workspace_path};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const STREAM_RESULT_LIMIT: usize = RESULT_LIMIT / 3;

#[derive(Debug)]
pub struct BashTool {
    workspace: PathBuf,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[schemars(range(min = 1, max = 600_000))]
    timeout_ms: Option<u64>,
    #[serde(default)]
    interactive: bool,
}
#[derive(Serialize)]
struct BashOutput {
    status: String,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    cancelled: bool,
    truncated: bool,
}

#[derive(Default)]
struct Captured {
    data: Vec<u8>,
    truncated: bool,
}
impl Captured {
    fn push(&mut self, chunk: &[u8]) {
        let remaining = STREAM_RESULT_LIMIT.saturating_sub(self.data.len());
        let copied = remaining.min(chunk.len());
        self.data.extend_from_slice(&chunk[..copied]);
        self.truncated |= copied < chunk.len();
    }
}

fn lossy_complete_prefix(bytes: &[u8]) -> String {
    let bytes = match std::str::from_utf8(bytes) {
        Ok(_) => bytes,
        Err(error) if error.error_len().is_none() => &bytes[..error.valid_up_to()],
        Err(_) => bytes,
    };
    String::from_utf8_lossy(bytes).into_owned()
}

impl BashTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace_path(workspace),
        }
    }
}
impl Default for BashTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    stream: OutputStream,
    progress: cookie_agent_engine::ProgressSink,
    captured: Arc<Mutex<Captured>>,
) -> Result<(), ToolError> {
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await.map_err(tool_error)?;
        if count == 0 {
            return Ok(());
        }
        progress.output(stream, &buffer[..count]);
        captured
            .lock()
            .expect("output capture lock poisoned")
            .push(&buffer[..count]);
    }
}

#[async_trait]
impl ToolProvider for BashTool {
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "bash".into(),
            description: "Run a shell command with streamed stdout/stderr.".into(),
            parameters: schema::<BashArgs>(),
        }])
    }
    async fn invoke(
        &self,
        mut ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<cookie_agent_engine::ToolResult, ToolError> {
        if call.name != "bash" {
            return Err(tool_error("bash tool received another tool name"));
        }
        let args: BashArgs = serde_json::from_value(call.arguments).map_err(tool_error)?;
        let timeout = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout == 0 || timeout > MAX_TIMEOUT_MS {
            return Err(tool_error("timeout_ms must be between 1 and 600000"));
        }
        let workspace = if ctx.cwd.as_os_str().is_empty() {
            workspace_for(&ctx, &self.workspace).to_owned()
        } else {
            ctx.cwd.clone()
        };
        let command = args.command.clone();
        let mut wrapped = CommandWrap::with_new("sh", move |process| {
            process
                .arg("-c")
                .arg(command)
                .current_dir(workspace)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        });
        wrapped.wrap(ProcessGroup::leader());
        let mut child = wrapped.spawn().map_err(tool_error)?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| tool_error("stdout was not piped"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| tool_error("stderr was not piped"))?;
        let stdout_capture = Arc::new(Mutex::new(Captured::default()));
        let stderr_capture = Arc::new(Mutex::new(Captured::default()));
        let stdout_task = tokio::spawn(drain(
            stdout,
            OutputStream::Stdout,
            ctx.progress.clone(),
            stdout_capture.clone(),
        ));
        let stderr_task = tokio::spawn(drain(
            stderr,
            OutputStream::Stderr,
            ctx.progress.clone(),
            stderr_capture.clone(),
        ));
        let mut stdin_task: Option<JoinHandle<Result<(), ToolError>>> = None;
        if args.interactive {
            let mut stdin = child
                .stdin()
                .take()
                .ok_or_else(|| tool_error("stdin was not piped"))?;
            if let Some(mut input) = ctx.stdin.take() {
                stdin_task = Some(tokio::spawn(async move {
                    while let Some(write) = input.recv().await {
                        if !write.data.is_empty() {
                            stdin.write_all(&write.data).await.map_err(tool_error)?;
                            stdin.flush().await.map_err(tool_error)?;
                        }
                        if write.eof {
                            return Ok(());
                        }
                    }
                    Ok(())
                }));
            }
        } else {
            child.stdin().take();
        }
        let (status, timed_out, cancelled) = tokio::select! {
            status = child.wait() => (status.map_err(tool_error)?, false, false),
            _ = tokio::time::sleep(Duration::from_millis(timeout)) => { child.start_kill().map_err(tool_error)?; (child.wait().await.map_err(tool_error)?, true, false) },
            _ = ctx.cancellation.cancelled() => { child.start_kill().map_err(tool_error)?; (child.wait().await.map_err(tool_error)?, false, true) },
        };
        if let Some(task) = stdin_task {
            task.abort();
        }
        stdout_task.await.map_err(tool_error)??;
        stderr_task.await.map_err(tool_error)??;
        let stdout = stdout_capture.lock().expect("output capture lock poisoned");
        let stderr = stderr_capture.lock().expect("output capture lock poisoned");
        let truncated = stdout.truncated || stderr.truncated;
        Ok(result(
            &BashOutput {
                status: if timed_out {
                    "timed_out".into()
                } else if cancelled {
                    "cancelled".into()
                } else {
                    "exited".into()
                },
                exit_code: status.code(),
                stdout: lossy_complete_prefix(&stdout.data),
                stderr: lossy_complete_prefix(&stderr.data),
                timed_out,
                cancelled,
                truncated,
            },
            truncated,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use cookie_agent_engine::{
        ProgressSink, ToolCall, ToolInvocationContext, ToolProvider, events::OutputMessage,
    };
    use cookie_agent_protocol::{OutputStream, RunId, SessionId, ToolCallId};
    use process_wrap::tokio::{CommandWrap, ProcessGroup};
    use tempfile::tempdir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::mpsc,
    };

    use super::BashTool;

    fn context(hub: cookie_agent_engine::events::OutputHub) -> ToolInvocationContext {
        let (progress, _) = mpsc::channel(1);
        ToolInvocationContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            cwd: std::env::current_dir().expect("current directory"),
            workspace_root: std::env::current_dir().expect("current directory"),
            progress: ProgressSink::new(progress, hub),
            cancellation: tokio_util::sync::CancellationToken::new(),
            stdin: None,
        }
    }

    #[tokio::test]
    async fn streams_stdout_and_stderr_as_distinct_deltas() {
        let directory = tempdir().expect("temporary directory");
        let tool = BashTool::new(directory.path());
        let call_id = ToolCallId::new_v7();
        let hub = cookie_agent_engine::events::OutputHub::new(call_id, 1024);
        let (_, mut stdout) = hub.subscribe(OutputStream::Stdout, 8);
        let (_, mut stderr) = hub.subscribe(OutputStream::Stderr, 8);
        let result = tool
            .invoke(
                context(hub),
                ToolCall {
                    id: call_id,
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"printf out; printf err >&2"}),
                },
            )
            .await
            .expect("bash result");
        assert!(result.content.contains("out"));
        match tokio::time::timeout(Duration::from_secs(1), stdout.recv())
            .await
            .expect("stdout delta timeout")
            .expect("stdout delta")
        {
            OutputMessage::Delta(delta) => assert_eq!(delta.stream, OutputStream::Stdout),
            OutputMessage::Gap(_) => panic!("unexpected stdout gap"),
        }
        match tokio::time::timeout(Duration::from_secs(1), stderr.recv())
            .await
            .expect("stderr delta timeout")
            .expect("stderr delta")
        {
            OutputMessage::Delta(delta) => assert_eq!(delta.stream, OutputStream::Stderr),
            OutputMessage::Gap(_) => panic!("unexpected stderr gap"),
        }
    }

    #[tokio::test]
    async fn timeout_kills_the_entire_process_group() {
        let directory = tempdir().expect("temporary directory");
        let pid_file = directory.path().join("child.pid");
        let tool = BashTool::new(directory.path());
        let call_id = ToolCallId::new_v7();
        let hub = cookie_agent_engine::events::OutputHub::new(call_id, 1024);
        let command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let result = tool
            .invoke(
                context(hub),
                ToolCall {
                    id: call_id,
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":command,"timeout_ms":50}),
                },
            )
            .await
            .expect("timeout result");
        assert!(result.content.contains("timed_out"));
        let pid = fs::read_to_string(pid_file)
            .expect("child pid")
            .trim()
            .to_owned();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "background child survived group kill"
        );
    }

    #[tokio::test]
    async fn stdin_round_trip_through_cat_like_process() {
        let mut command = CommandWrap::with_new("sh", |process| {
            process
                .arg("-c")
                .arg("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
        });
        command.wrap(ProcessGroup::leader());
        let mut child = command.spawn().expect("spawn cat");
        let mut stdin = child.stdin().take().expect("stdin");
        let mut stdout = child.stdout().take().expect("stdout");
        stdin.write_all(b"round trip\n").await.expect("write stdin");
        drop(stdin);
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .await
            .expect("read stdout");
        child.wait().await.expect("reap cat");
        assert_eq!(output, "round trip\n");
    }

    #[tokio::test]
    async fn capped_valid_utf8_output_discards_only_the_partial_codepoint() {
        let directory = tempdir().expect("temporary directory");
        let tool = BashTool::new(directory.path());
        let call_id = ToolCallId::new_v7();
        let hub = cookie_agent_engine::events::OutputHub::new(call_id, 1024);
        let prefix = super::STREAM_RESULT_LIMIT - 2;
        let result = tool
            .invoke(
                context(hub),
                ToolCall {
                    id: call_id,
                    name: "bash".into(),
                    arguments: serde_json::json!({
                        "command": format!(
                            "printf '%*s' {prefix} '' | tr ' ' a; printf '\\360\\237\\230\\200'"
                        )
                    }),
                },
            )
            .await
            .expect("bash result");
        let output: serde_json::Value = serde_json::from_str(&result.content).expect("bash JSON");

        assert_eq!(output["stdout"], "a".repeat(prefix));
        assert!(
            !output["stdout"]
                .as_str()
                .expect("stdout")
                .contains('\u{fffd}')
        );
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn invalid_process_bytes_are_lossy_safe() {
        let directory = tempdir().expect("temporary directory");
        let tool = BashTool::new(directory.path());
        let call_id = ToolCallId::new_v7();
        let result = tool
            .invoke(
                context(cookie_agent_engine::events::OutputHub::new(call_id, 1024)),
                ToolCall {
                    id: call_id,
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"printf '\\377bad'"}),
                },
            )
            .await
            .expect("bash result");
        let output: serde_json::Value = serde_json::from_str(&result.content).expect("bash JSON");

        assert_eq!(output["stdout"], "�bad");
    }
}
