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

use std::{collections::BTreeMap, process::Stdio, sync::Arc, time::Duration};

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

/// How long a new process waits for the previous one's [`ProcessLifecycle::stopped`]
/// to finish, such as a `stop_command` removing a container whose name the
/// new one takes.
const STOPPED_WAIT: Duration = Duration::from_secs(90);

/// What a connector does around each process it runs: a temporary
/// runtime's record and `stop_command`
/// (`docs/site/specs/temporary-runtimes.md` section 5).
pub(crate) trait ProcessLifecycle: Send + Sync {
    /// The process started.
    fn started(&self);

    /// The process has exited or been killed; runs to completion before
    /// the connector starts another.
    fn stopped(&self) -> futures_util::future::BoxFuture<'static, ()>;
}

/// Starts a runtime per connection by running a command.
pub(crate) struct ProcessConnector {
    /// The program and arguments.
    command: Vec<String>,
    env: BTreeMap<String, String>,
    /// The working directory; the daemon's when `None`.
    cwd: Option<std::path::PathBuf>,
    shutdown_grace: Duration,
    lifecycle: Option<Arc<dyn ProcessLifecycle>>,
    /// How many processes' [`ProcessLifecycle::stopped`] is still running.
    stopping: Arc<tokio::sync::watch::Sender<usize>>,
    /// How many processes this connector ran have not finished stopping,
    /// [`ProcessLifecycle::stopped`] included.
    running: Arc<tokio::sync::watch::Sender<usize>>,
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
            lifecycle: None,
            stopping: Arc::new(tokio::sync::watch::channel(0).0),
            running: Arc::new(tokio::sync::watch::channel(0).0),
        }
    }

    /// Tells `lifecycle` of each process it runs.
    pub(crate) fn with_lifecycle(mut self, lifecycle: Arc<dyn ProcessLifecycle>) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }

    /// Waits until every process this connector ran that has begun
    /// stopping has finished, its [`ProcessLifecycle::stopped`] included,
    /// up to [`STOPPED_WAIT`].
    async fn stops_settled(&self) {
        let mut stopping = self.stopping.subscribe();
        let _ = tokio::time::timeout(STOPPED_WAIT, stopping.wait_for(|count| *count == 0)).await;
    }

    /// Waits until every process this connector ran has stopped, its
    /// [`ProcessLifecycle::stopped`] included, up to [`STOPPED_WAIT`]: for
    /// a caller that has let go of every connection.
    pub(crate) async fn settled(&self) {
        let mut running = self.running.subscribe();
        let _ = tokio::time::timeout(STOPPED_WAIT, running.wait_for(|count| *count == 0)).await;
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
        // The previous process's cleanup, such as removing its container,
        // comes first.
        self.stops_settled().await;
        let mut command = tokio::process::Command::new(
            cookie_agent_protocol::paths::resolve_program(program, &self.env),
        );
        // The user's shell environment of this machine, under the
        // configured `env`.
        let environment = cookie_agent_runtime_api::shell_environment::environment().await;
        cookie_agent_runtime_api::shell_environment::apply(environment.as_deref(), &mut command);
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
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.started();
        }
        self.running.send_modify(|count| *count += 1);
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
                lifecycle: Some(Stopping {
                    lifecycle: self.lifecycle.clone(),
                    stopping: Arc::clone(&self.stopping),
                    running: Arc::clone(&self.running),
                }),
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
    lifecycle: Option<Stopping>,
}

/// A process's [`ProcessLifecycle`], with its connector's counts of
/// processes running and stopping.
struct Stopping {
    lifecycle: Option<Arc<dyn ProcessLifecycle>>,
    stopping: Arc<tokio::sync::watch::Sender<usize>>,
    running: Arc<tokio::sync::watch::Sender<usize>>,
}

impl Stopping {
    /// Counts the process as stopping now, and returns the work that ends
    /// its stop: the lifecycle's, then the counts.
    fn begin(self) -> impl std::future::Future<Output = ()> + Send + 'static {
        self.stopping.send_modify(|count| *count += 1);
        let stopped = self.lifecycle.as_ref().map(|lifecycle| lifecycle.stopped());
        async move {
            if let Some(stopped) = stopped {
                stopped.await;
            }
            self.stopping
                .send_modify(|count| *count = count.saturating_sub(1));
            self.running
                .send_modify(|count| *count = count.saturating_sub(1));
        }
    }
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
        let child = self
            .child
            .take()
            .and_then(|mut child| (!matches!(child.try_wait(), Ok(Some(_)))).then_some(child));
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Without a runtime to run it on, the cleanup is left to the
            // next daemon's recovery, which finds the process still
            // recorded.
            if let Some(mut child) = child {
                let _ = child.start_kill();
            }
            return;
        };
        let stopped = self.lifecycle.take().map(Stopping::begin);
        let grace = self.shutdown_grace;
        // The stdin pipe closes when the fields drop, right after this.
        handle.spawn(async move {
            if let Some(child) = child {
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
            }
            if let Some(stopped) = stopped {
                stopped.await;
            }
        });
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
