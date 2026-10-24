//! Runtimes in child processes: the connector a configured
//! `[runtimes.<id>]` template starts its instances with.
//!
//! [`ProcessConnector`] spawns the template's command in its own process
//! group (a job object on Windows), with the daemon's environment
//! plus the template's `env`. The runtime protocol runs over the child's
//! stdout and stdin, one message per line. The child's stderr is captured
//! into a bounded [`DiagnosticTail`] that failure reasons end with.
//!
//! When the connection ends, the child's stdin closes, which stops a
//! runtime that is still running; it then gets the template's shutdown grace
//! to exit before its process group is killed.

use std::{collections::BTreeMap, process::Stdio, time::Duration};

use async_trait::async_trait;
use cookie_agent_protocol::{MessageFrame, Transport, TransportError};
use cookie_agent_runtime_api::{
    DiagnosticTail, RuntimeConnection, RuntimeConnector, rpc::LineTransport,
};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use tokio::{
    io::AsyncReadExt as _,
    process::{ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

/// How long a transport whose stdout ended waits for the process to exit
/// and its stderr to drain before it reports the exit.
const EXIT_REPORT_WAIT: Duration = Duration::from_secs(2);

/// Starts a runtime per connection by running a command.
pub(crate) struct ProcessConnector {
    /// The program and arguments.
    command: Vec<String>,
    env: BTreeMap<String, String>,
    /// The working directory; the daemon's when `None`.
    cwd: Option<std::path::PathBuf>,
    shutdown_grace: Duration,
}

impl ProcessConnector {
    pub(crate) fn new(
        command: Vec<String>,
        env: BTreeMap<String, String>,
        shutdown_grace: Duration,
    ) -> Self {
        Self {
            command,
            env,
            cwd: None,
            shutdown_grace,
        }
    }

    /// Runs the command in `cwd`.
    pub(crate) fn with_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }
}

#[async_trait]
impl RuntimeConnector for ProcessConnector {
    async fn connect(&self) -> Result<RuntimeConnection, String> {
        let (program, arguments) = self.command.split_first().ok_or("the command is empty")?;
        let mut command = tokio::process::Command::new(
            cookie_agent_protocol::paths::resolve_program(program, &self.env),
        );
        command
            .args(arguments)
            .envs(&self.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        let mut wrapped = CommandWrap::from(command);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        let mut child = wrapped
            .spawn()
            .map_err(|error| format!("failed to run `{program}`: {error}"))?;
        let stdin = child.stdin().take();
        let stdout = child.stdout().take();
        let stderr = child.stderr().take();
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            let _ = child.start_kill();
            return Err("the process has no stdio pipes".into());
        };
        let diagnostics = DiagnosticTail::default();
        let stderr_done = CancellationToken::new();
        tokio::spawn(capture_stderr(
            stderr,
            diagnostics.clone(),
            stderr_done.clone(),
        ));
        Ok(RuntimeConnection {
            transport: Box::new(ProcessTransport {
                lines: LineTransport::new(stdout, stdin),
                child: Some(child),
                stderr_done,
                shutdown_grace: self.shutdown_grace,
            }),
            diagnostics: Some(diagnostics),
        })
    }
}

/// Reads the child's stderr into `tail` line by line, keeping at most
/// [`DiagnosticTail::MAX_LINE_BYTES`] of each line in memory.
async fn capture_stderr(
    mut stderr: tokio::process::ChildStderr,
    tail: DiagnosticTail,
    done: CancellationToken,
) {
    let mut buffer = [0_u8; 4096];
    let mut line = Vec::with_capacity(256);
    loop {
        let read = match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                tail.push_line(&String::from_utf8_lossy(&line));
                line.clear();
            } else if line.len() < DiagnosticTail::MAX_LINE_BYTES * 4 {
                line.push(*byte);
            }
        }
    }
    if !line.is_empty() {
        tail.push_line(&String::from_utf8_lossy(&line));
    }
    done.cancel();
}

/// The runtime protocol over a child's stdio. Owns the child: dropping the
/// transport closes its stdin, waits up to the shutdown grace for it to exit,
/// and then kills its process group.
struct ProcessTransport {
    lines: LineTransport<ChildStdout, ChildStdin>,
    child: Option<Box<dyn ChildWrapper>>,
    stderr_done: CancellationToken,
    shutdown_grace: Duration,
}

impl ProcessTransport {
    /// Why the child's stdout ended: its exit status, once it has exited
    /// and its stderr has drained (both briefly awaited).
    async fn exit_reason(&mut self) -> String {
        let status = match self.child.as_mut() {
            Some(child) => tokio::time::timeout(EXIT_REPORT_WAIT, child.wait()).await,
            None => return "the runtime process exited".into(),
        };
        let _ = tokio::time::timeout(EXIT_REPORT_WAIT, self.stderr_done.cancelled()).await;
        match status {
            Ok(Ok(status)) => {
                self.child = None;
                format!("the runtime process exited ({status})")
            }
            Ok(Err(error)) => format!("the runtime process closed its output: {error}"),
            Err(_) => "the runtime process closed its output".into(),
        }
    }
}

#[async_trait]
impl Transport for ProcessTransport {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let result = self.lines.send(frame).await;
        if result.is_err() && self.child.is_some() {
            // A child that exits before reading its input breaks the pipe:
            // its exit status and stderr say why, the broken pipe does not.
            let reason = self.exit_reason().await;
            if self.child.is_none() {
                return Err(TransportError::Other(reason));
            }
        }
        result
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        match self.lines.recv().await {
            Ok(None) => Err(TransportError::Other(self.exit_reason().await)),
            other => other,
        }
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let grace = self.shutdown_grace;
        match tokio::runtime::Handle::try_current() {
            // The stdin pipe closes when the fields drop, right after this.
            Ok(handle) => {
                handle.spawn(async move {
                    let mut reaper = Reaper {
                        child,
                        exited: false,
                    };
                    if tokio::time::timeout(grace, reaper.child.wait())
                        .await
                        .is_ok()
                    {
                        reaper.exited = true;
                    }
                });
            }
            Err(_) => {
                let _ = child.start_kill();
            }
        }
    }
}

/// Kills the process group of a child that has not exited when it drops:
/// after the grace, or when the daemon's runtime stops first.
struct Reaper {
    child: Box<dyn ChildWrapper>,
    exited: bool,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.start_kill();
            let _ = self.child.try_wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
