//! The runtime's end of the ACP agent relay
//! (`docs/site/specs/acp-agents.md` section 7).
//!
//! A runtime hosts the ACP agents the daemon declares in
//! `runtime/initialize` (a template's, or `[runtimes.local]` for the
//! daemon's own runtime), else its own machine's, and reports them in its
//! initialize result. `agent/start` runs an agent's command without a shell
//! in the runtime's home directory, in its own process group (a job object
//! on Windows), with the runtime's environment and the agent's `env`. Each
//! line the agent writes to its standard output goes to the daemon as
//! `agent/message`, unchanged; each `agent/message` from the daemon becomes
//! one line of its standard input. Its standard error is kept as a bounded
//! tail, which `agent/exited` carries when the process ends. `agent/stop`
//! and the end of the connection kill it.
//!
//! `agent/mcp_open` opens a bridge socket ([`crate::acp_bridge`]) for one of
//! the agent's sessions; each connection to it is numbered, and its lines
//! travel as `agent/mcp` both ways until it ends (`agent/mcp_closed`) or
//! the daemon closes the bridge (`agent/mcp_close`).

use std::{
    collections::{BTreeMap, HashMap},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use cookie_agent_protocol::errors;
use cookie_agent_runtime_api::{
    DiagnosticTail,
    protocol::{
        AcpAgentReport, AcpAgentSettings, AgentExitedParams, AgentMcpBridgeRef,
        AgentMcpClosedParams, AgentMcpOpenResult, AgentMcpParams, AgentMessageParams, AgentRef,
        AgentsChangedParams, Empty, method,
    },
    rpc::{IncomingNotification, IncomingRequest, Peer, RpcError},
};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use crate::acp_bridge::{BridgeListener, BridgeStream};

/// The longest line an agent or a bridge connection may send: a runtime
/// protocol frame.
const MAX_LINE_BYTES: usize = cookie_agent_runtime_api::protocol::MAX_FRAME_BYTES;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn agent_error(agent: &str, detail: impl Into<String>) -> RpcError {
    errors::AGENT_FAILED
        .error()
        .field("agent", agent)
        .detail(detail.into())
        .into()
}

/// The ACP agents of one runtime connection and their processes.
pub(crate) struct AcpHost {
    peer: Peer,
    /// The argument vector `--socket <path>` is appended to, to make a
    /// bridge's command.
    bridge_command: Vec<String>,
    agents: Mutex<BTreeMap<String, AcpAgentSettings>>,
    processes: Mutex<HashMap<String, Arc<AgentProcess>>>,
    bridges: Mutex<HashMap<String, Arc<Bridge>>>,
    next_bridge: AtomicU64,
    /// Cancelled when the connection ends: every process and bridge stops.
    stop: CancellationToken,
}

struct AgentProcess {
    /// Lines to write to its standard input.
    input: mpsc::UnboundedSender<String>,
    /// Kills it.
    kill: CancellationToken,
    /// The bridges opened for it, closed with it.
    bridges: Mutex<Vec<String>>,
}

struct Bridge {
    agent: String,
    /// Lines to each connection, by number.
    connections: Mutex<HashMap<u64, mpsc::UnboundedSender<String>>>,
    close: CancellationToken,
}

impl AcpHost {
    pub(crate) fn new(peer: Peer, bridge_command: Vec<String>, stop: CancellationToken) -> Self {
        Self {
            peer,
            bridge_command,
            agents: Mutex::default(),
            processes: Mutex::default(),
            bridges: Mutex::default(),
            next_bridge: AtomicU64::new(1),
            stop,
        }
    }

    /// Takes the declarations, once, at `runtime/initialize`.
    pub(crate) fn declare(&self, agents: Vec<AcpAgentSettings>) -> Result<(), String> {
        let agents = validated(agents)?;
        *lock(&self.agents) = agents;
        Ok(())
    }

    /// The agents as the initialize result and `runtime/agents_changed`
    /// report them, in ID order.
    pub(crate) fn reports(&self) -> Vec<AcpAgentReport> {
        lock(&self.agents)
            .values()
            .map(AcpAgentReport::from)
            .collect()
    }

    /// Replaces the declarations with `agents`, stopping the processes of
    /// agents removed or changed, and reports the change.
    pub(crate) async fn replace(&self, agents: Vec<AcpAgentSettings>) -> Result<(), String> {
        let agents = validated(agents)?;
        let stale = {
            let mut current = lock(&self.agents);
            let stale = current
                .iter()
                .filter(|(id, agent)| agents.get(*id) != Some(agent))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            *current = agents;
            stale
        };
        for agent in stale {
            self.stop_agent(&agent);
        }
        let _ = self
            .peer
            .notify(
                method::AGENTS_CHANGED,
                &AgentsChangedParams {
                    agents: self.reports(),
                },
            )
            .await;
        Ok(())
    }

    /// Answers the `agent/*` requests; `None` for any other method.
    pub(crate) async fn handle_request(
        self: &Arc<Self>,
        request: &IncomingRequest,
    ) -> Option<Result<serde_json::Value, RpcError>> {
        let result = match request.method.as_str() {
            method::AGENT_START => match request.params::<AgentRef>() {
                Ok(agent) => self.start(&agent.agent).map(|()| json_empty()),
                Err(error) => Err(error),
            },
            method::AGENT_STOP => match request.params::<AgentRef>() {
                Ok(agent) => {
                    self.stop_agent(&agent.agent);
                    Ok(json_empty())
                }
                Err(error) => Err(error),
            },
            method::AGENT_MCP_OPEN => match request.params::<AgentRef>() {
                Ok(agent) => self.open_bridge(&agent.agent).and_then(|result| {
                    serde_json::to_value(result).map_err(|error| {
                        RpcError::Protocol(format!("bridge result does not serialize: {error}"))
                    })
                }),
                Err(error) => Err(error),
            },
            _ => return None,
        };
        Some(result)
    }

    /// Handles the `agent/*` notifications; `false` for any other method.
    pub(crate) fn handle_notification(&self, notification: &IncomingNotification) -> bool {
        match notification.method.as_str() {
            method::AGENT_MESSAGE => {
                match notification.params::<AgentMessageParams>() {
                    Ok(params) => {
                        let process = lock(&self.processes).get(&params.agent).cloned();
                        match process {
                            Some(process) => {
                                let _ = process.input.send(params.message.to_string());
                            }
                            None => eprintln!(
                                "cookie runtime: a message for ACP agent `{}`, which is not \
                                 running, was dropped",
                                params.agent
                            ),
                        }
                    }
                    Err(error) => eprintln!("cookie runtime: unreadable `agent/message`: {error}"),
                }
                true
            }
            method::AGENT_MCP => {
                match notification.params::<AgentMcpParams>() {
                    Ok(params) => {
                        let sender = lock(&self.bridges).get(&params.bridge).and_then(|bridge| {
                            lock(&bridge.connections).get(&params.connection).cloned()
                        });
                        if let Some(sender) = sender {
                            let _ = sender.send(params.message.to_string());
                        }
                    }
                    Err(error) => eprintln!("cookie runtime: unreadable `agent/mcp`: {error}"),
                }
                true
            }
            method::AGENT_MCP_CLOSE => {
                if let Ok(params) = notification.params::<AgentMcpBridgeRef>() {
                    self.close_bridge(&params.bridge);
                }
                true
            }
            _ => false,
        }
    }

    fn start(self: &Arc<Self>, agent: &str) -> Result<(), RpcError> {
        let settings = lock(&self.agents).get(agent).cloned().ok_or_else(|| {
            agent_error(agent, format!("the runtime hosts no ACP agent `{agent}`"))
        })?;
        if !settings.enabled {
            return Err(agent_error(
                agent,
                format!("ACP agent `{agent}` is disabled"),
            ));
        }
        let mut processes = lock(&self.processes);
        if processes.contains_key(agent) {
            return Ok(());
        }
        let (program, arguments) = settings
            .command
            .split_first()
            .ok_or_else(|| agent_error(agent, "the agent's command is empty"))?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(arguments)
            .envs(&settings.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(home) = cookie_agent_protocol::paths::home_dir() {
            command.current_dir(home);
        }
        let mut wrapped = CommandWrap::from(command);
        #[cfg(unix)]
        wrapped.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        wrapped.wrap(JobObject);
        let mut child = wrapped
            .spawn()
            .map_err(|error| agent_error(agent, format!("failed to run `{program}`: {error}")))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            child.stdin().take(),
            child.stdout().take(),
            child.stderr().take(),
        ) else {
            let _ = child.start_kill();
            return Err(agent_error(agent, "the process has no stdio pipes"));
        };
        let (input, input_rx) = mpsc::unbounded_channel();
        let kill = self.stop.child_token();
        let process = Arc::new(AgentProcess {
            input,
            kill: kill.clone(),
            bridges: Mutex::default(),
        });
        processes.insert(agent.to_owned(), Arc::clone(&process));
        drop(processes);
        let tail = DiagnosticTail::default();
        let stderr_done = CancellationToken::new();
        tokio::spawn(capture_stderr(stderr, tail.clone(), stderr_done.clone()));
        tokio::spawn(write_lines(stdin, input_rx, kill.clone()));
        let host = Arc::clone(self);
        let name = agent.to_owned();
        tokio::spawn(async move {
            host.relay_output(&name, stdout, &tail).await;
            let (status, message) = wait_for(child, &kill).await;
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), stderr_done.cancelled())
                    .await;
            host.exited(&name, &process, status, message, &tail).await;
        });
        Ok(())
    }

    /// Sends each line of the agent's standard output to the daemon until
    /// it ends or the process is killed.
    async fn relay_output(
        &self,
        agent: &str,
        stdout: tokio::process::ChildStdout,
        tail: &DiagnosticTail,
    ) {
        let kill = lock(&self.processes)
            .get(agent)
            .map(|process| process.kill.clone())
            .unwrap_or_default();
        let mut lines = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = tokio::select! {
                () = kill.cancelled() => return,
                read = read_line(&mut lines, &mut line) => read,
            };
            match read {
                Ok(false) | Err(_) => return,
                Ok(true) => {}
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(text) {
                Ok(message) => {
                    let params = AgentMessageParams {
                        agent: agent.to_owned(),
                        message,
                    };
                    if self
                        .peer
                        .notify(method::AGENT_MESSAGE, &params)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                // Only protocol messages belong on standard output; anything
                // else is kept with the diagnostics.
                Err(_) => tail.push_line(text),
            }
        }
    }

    async fn exited(
        &self,
        agent: &str,
        process: &Arc<AgentProcess>,
        status: Option<i32>,
        message: Option<String>,
        tail: &DiagnosticTail,
    ) {
        {
            let mut processes = lock(&self.processes);
            if processes
                .get(agent)
                .is_some_and(|current| Arc::ptr_eq(current, process))
            {
                processes.remove(agent);
            }
        }
        for bridge in std::mem::take(&mut *lock(&process.bridges)) {
            self.close_bridge(&bridge);
        }
        if self.stop.is_cancelled() {
            return;
        }
        let _ = self
            .peer
            .notify(
                method::AGENT_EXITED,
                &AgentExitedParams {
                    agent: agent.to_owned(),
                    status,
                    message,
                    stderr: tail.text().unwrap_or_default(),
                },
            )
            .await;
    }

    fn stop_agent(&self, agent: &str) {
        if let Some(process) = lock(&self.processes).get(agent) {
            process.kill.cancel();
        }
    }

    fn open_bridge(self: &Arc<Self>, agent: &str) -> Result<AgentMcpOpenResult, RpcError> {
        let process = lock(&self.processes)
            .get(agent)
            .cloned()
            .ok_or_else(|| agent_error(agent, format!("ACP agent `{agent}` is not running")))?;
        let listener = BridgeListener::bind().map_err(|error| {
            agent_error(agent, format!("failed to open a bridge socket: {error}"))
        })?;
        let bridge_id = format!("b{}", self.next_bridge.fetch_add(1, Ordering::Relaxed));
        let mut command = self.bridge_command.clone();
        command.extend(["--socket".to_owned(), listener.path().to_owned()]);
        let close = process.kill.child_token();
        let bridge = Arc::new(Bridge {
            agent: agent.to_owned(),
            connections: Mutex::default(),
            close: close.clone(),
        });
        lock(&self.bridges).insert(bridge_id.clone(), Arc::clone(&bridge));
        lock(&process.bridges).push(bridge_id.clone());
        let host = Arc::clone(self);
        let id = bridge_id.clone();
        tokio::spawn(async move {
            let mut next_connection = 1_u64;
            loop {
                let stream = tokio::select! {
                    () = close.cancelled() => break,
                    stream = listener.accept() => stream,
                };
                let Ok(stream) = stream else {
                    break;
                };
                let connection = next_connection;
                next_connection += 1;
                let (sender, receiver) = mpsc::unbounded_channel();
                lock(&bridge.connections).insert(connection, sender);
                tokio::spawn(Arc::clone(&host).serve_connection(
                    id.clone(),
                    Arc::clone(&bridge),
                    connection,
                    stream,
                    receiver,
                ));
            }
            drop(listener);
            lock(&host.bridges).remove(&id);
        });
        Ok(AgentMcpOpenResult {
            bridge: bridge_id,
            command,
        })
    }

    async fn serve_connection(
        self: Arc<Self>,
        bridge_id: String,
        bridge: Arc<Bridge>,
        connection: u64,
        stream: Box<dyn BridgeStream>,
        mut outgoing: mpsc::UnboundedReceiver<String>,
    ) {
        let (read, mut write) = tokio::io::split(stream);
        let close = bridge.close.clone();
        let writer = async {
            while let Some(line) = outgoing.recv().await {
                if write.write_all(line.as_bytes()).await.is_err()
                    || write.write_all(b"\n").await.is_err()
                    || write.flush().await.is_err()
                {
                    break;
                }
            }
        };
        let reader = async {
            let mut lines = BufReader::new(read);
            let mut line = Vec::new();
            loop {
                line.clear();
                match read_line(&mut lines, &mut line).await {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
                let text = String::from_utf8_lossy(&line);
                let Ok(message) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
                    continue;
                };
                let params = AgentMcpParams {
                    bridge: bridge_id.clone(),
                    connection,
                    message,
                };
                if self.peer.notify(method::AGENT_MCP, &params).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            () = close.cancelled() => {}
            () = writer => {}
            () = reader => {}
        }
        lock(&bridge.connections).remove(&connection);
        if !self.stop.is_cancelled() {
            let _ = self
                .peer
                .notify(
                    method::AGENT_MCP_CLOSED,
                    &AgentMcpClosedParams {
                        bridge: bridge_id,
                        connection,
                    },
                )
                .await;
        }
        let _ = &bridge.agent;
    }

    fn close_bridge(&self, bridge: &str) {
        if let Some(bridge) = lock(&self.bridges).remove(bridge) {
            bridge.close.cancel();
        }
    }
}

fn json_empty() -> serde_json::Value {
    serde_json::to_value(Empty {}).expect("an empty object serializes")
}

/// The declarations by ID, each checked as the daemon's configuration
/// checks them.
fn validated(agents: Vec<AcpAgentSettings>) -> Result<BTreeMap<String, AcpAgentSettings>, String> {
    let mut by_id = BTreeMap::new();
    for agent in agents {
        if cookie_agent_protocol::AgentId::new(agent.id.as_str()).is_err() {
            return Err(format!("ACP agent ID `{}` is not an agent ID", agent.id));
        }
        if agent.command.first().is_none_or(String::is_empty) {
            return Err(format!("ACP agent `{}` has no command", agent.id));
        }
        if by_id.insert(agent.id.clone(), agent).is_some() {
            return Err("an ACP agent is declared twice".to_owned());
        }
    }
    Ok(by_id)
}

/// Reads one line into `line`, without its terminator; `false` at the end
/// of input. A line longer than [`MAX_LINE_BYTES`] is an error.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    let mut limited = reader.take(MAX_LINE_BYTES as u64 + 1);
    let read = limited.read_until(b'\n', line).await?;
    if read == 0 {
        return Ok(false);
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::other("line exceeds the frame bound"));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(true)
}

async fn write_lines(
    mut stdin: tokio::process::ChildStdin,
    mut lines: mpsc::UnboundedReceiver<String>,
    kill: CancellationToken,
) {
    loop {
        let line = tokio::select! {
            () = kill.cancelled() => return,
            line = lines.recv() => line,
        };
        let Some(line) = line else {
            return;
        };
        if stdin.write_all(line.as_bytes()).await.is_err()
            || stdin.write_all(b"\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            return;
        }
    }
}

/// Reads the agent's stderr into `tail` line by line.
async fn capture_stderr(
    stderr: tokio::process::ChildStderr,
    tail: DiagnosticTail,
    done: CancellationToken,
) {
    let mut lines = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line(&mut lines, &mut line).await {
            Ok(true) => tail.push_line(&String::from_utf8_lossy(&line)),
            Ok(false) | Err(_) => break,
        }
    }
    done.cancel();
}

/// Waits for the agent to exit, killing its process group when `kill` is
/// cancelled first. Returns its exit code, or why it has none.
async fn wait_for(
    mut child: Box<dyn ChildWrapper>,
    kill: &CancellationToken,
) -> (Option<i32>, Option<String>) {
    let status = tokio::select! {
        status = child.wait() => status,
        () = kill.cancelled() => {
            let _ = child.start_kill();
            let status = child.wait().await;
            return (
                status.ok().and_then(|status| status.code()),
                Some("stopped".to_owned()),
            );
        }
    };
    match status {
        Ok(status) => (
            status.code(),
            status
                .code()
                .is_none()
                .then(|| format!("the process ended ({status})")),
        ),
        Err(error) => (
            None,
            Some(format!("waiting for the process failed: {error}")),
        ),
    }
}
