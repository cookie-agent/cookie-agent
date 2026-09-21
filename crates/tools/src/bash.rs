use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

#[cfg(any(unix, windows))]
use std::ffi::OsString;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;
#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use async_trait::async_trait;
use cookie_agent_engine::ToolProgress;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec, ToolStdin,
};
use cookie_agent_protocol::PersistedToolResult as ToolResult;
use cookie_agent_protocol::{ApprovalResourceSource, PermissionAction, PreparedBindingLifetime};
use cookie_agent_protocol::{OutputStream, SafeDisplayText, ToolCallId};
#[cfg(windows)]
use process_wrap::tokio::{CommandWrap, JobObject as ProcessJobObject, KillOnDrop};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::process::Child;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

use crate::{fs_cap, parse_args, prepared_operation, prepared_resource, schema};

#[derive(Debug)]
pub struct BashTool {
    _workspace: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct BashArgs {
    command: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default)]
    interactive: bool,
}

fn default_timeout() -> u64 {
    120_000
}

struct BashExecutor {
    tool_call_id: ToolCallId,
    args: BashArgs,
    cwd: fs_cap::PreparedExisting,
    executable: fs_cap::PreparedExisting,
}

pub const OUTPUT_CHUNK_FLUSH_BYTES: usize = 4 * 1024;
pub const OUTPUT_CHUNK_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct MergedOutput {
    text: String,
    truncated: bool,
}

impl MergedOutput {
    fn append(&mut self, chunk: &str) {
        if self.truncated {
            return;
        }
        let room = cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES.saturating_sub(self.text.len());
        let mut end = chunk.len().min(room);
        while !chunk.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&chunk[..end]);
        self.truncated = end < chunk.len();
    }
}

fn sanitized_chunks(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for character in text.chars() {
        let character = if character.is_control() && !matches!(character, '\n' | '\t') {
            ' '
        } else {
            character
        };
        if chunk.len() + character.len_utf8() > SafeDisplayText::MAX_BYTES {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

async fn emit_preview(
    progress: &ProgressSink,
    tool_call_id: ToolCallId,
    stream: &OutputStream,
    bytes: &[u8],
) -> Result<(), ToolError> {
    let stream_name = stream.name();
    for chunk in sanitized_chunks(bytes) {
        progress
            .send(ToolProgress {
                output: Vec::new(),
                tool_call_id,
                message: format!("bash {stream_name}"),
                display: Some(chunk),
            })
            .await?;
    }
    Ok(())
}

async fn read_output<R>(
    mut reader: R,
    stream: OutputStream,
    progress: ProgressSink,
    tool_call_id: ToolCallId,
    merged: Arc<Mutex<MergedOutput>>,
) -> Result<(), ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut pending = Vec::new();
    let mut undecoded = Vec::new();
    let mut read_buffer = [0_u8; OUTPUT_CHUNK_FLUSH_BYTES];
    let flush = tokio::time::sleep(OUTPUT_CHUNK_FLUSH_INTERVAL);
    tokio::pin!(flush);
    loop {
        tokio::select! {
            read = reader.read(&mut read_buffer) => {
                let count = read.map_err(|error| ToolError::execution(error.to_string()))?;
                undecoded.extend_from_slice(&read_buffer[..count]);
                let text = decode_output(&mut undecoded, count == 0);
                {
                    // Record read arrival, independently of the per-pipe preview batches.
                    // Separate pipes cannot reconstruct the process's exact write order.
                    let mut merged = merged.lock().await;
                    for chunk in sanitized_chunks(text.as_bytes()) {
                        merged.append(&chunk);
                    }
                }
                if count == 0 {
                    if !pending.is_empty() {
                        flush_output(
                            &progress,
                            tool_call_id,
                            &stream,
                            (&mut pending, true),
                        ).await?;
                    }
                    return Ok(());
                }
                let bytes = &read_buffer[..count];
                if pending.is_empty() {
                    flush.as_mut().reset(tokio::time::Instant::now() + OUTPUT_CHUNK_FLUSH_INTERVAL);
                }
                pending.extend_from_slice(bytes);
                if pending.len() >= OUTPUT_CHUNK_FLUSH_BYTES {
                    flush_output(
                        &progress,
                        tool_call_id,
                        &stream,
                        (&mut pending, false),
                    ).await?;
                }
            }
            () = &mut flush, if !pending.is_empty() => {
                flush_output(
                    &progress,
                    tool_call_id,
                    &stream,
                    (&mut pending, false),
                ).await?;
                flush.as_mut().reset(tokio::time::Instant::now() + OUTPUT_CHUNK_FLUSH_INTERVAL);
            }
        }
    }
}

fn decode_output(pending: &mut Vec<u8>, eof: bool) -> String {
    let mut text = String::new();
    let mut consumed = 0;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(valid) => {
                text.push_str(valid);
                consumed = pending.len();
            }
            Err(error) => {
                let end = consumed + error.valid_up_to();
                text.push_str(
                    std::str::from_utf8(&pending[consumed..end]).expect("valid UTF-8 prefix"),
                );
                consumed = end;
                if let Some(length) = error.error_len() {
                    text.push('\u{fffd}');
                    consumed += length;
                } else if eof {
                    text.push('\u{fffd}');
                    consumed = pending.len();
                } else {
                    break;
                }
            }
        }
    }
    pending.drain(..consumed);
    text
}

async fn flush_output(
    progress: &ProgressSink,
    tool_call_id: ToolCallId,
    stream: &OutputStream,
    pending: (&mut Vec<u8>, bool),
) -> Result<(), ToolError> {
    let (pending, eof) = pending;
    let text = decode_output(pending, eof);
    if text.is_empty() {
        return Ok(());
    }
    let name = stream.name();
    progress
        .send(ToolProgress {
            tool_call_id,
            message: String::new(),
            display: None,
            output: vec![cookie_agent_protocol::ToolOutputChunk {
                stream: Some(name.into()),
                text: text.clone(),
            }],
        })
        .await?;
    emit_preview(progress, tool_call_id, stream, text.as_bytes()).await
}

#[cfg(unix)]
struct ProcessGroupChild {
    child: Option<Child>,
    process_group: i32,
    complete: bool,
}

#[cfg(unix)]
impl ProcessGroupChild {
    fn kill_group(&mut self) {
        unsafe {
            libc::kill(-self.process_group, libc::SIGKILL);
        }
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }

    async fn kill_and_reap(&mut self) {
        self.kill_group();
        if let Some(child) = &mut self.child {
            let _ = child.wait().await;
        }
        self.complete = true;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupChild {
    fn drop(&mut self) {
        if !self.complete {
            self.kill_group();
            if let Some(mut child) = self.child.take()
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}

#[cfg(windows)]
struct JobChild {
    child: Option<Box<dyn process_wrap::tokio::ChildWrapper>>,
    complete: bool,
}

#[cfg(windows)]
impl JobChild {
    fn kill_group(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }

    async fn kill_and_reap(&mut self) {
        self.kill_group();
        if let Some(child) = &mut self.child {
            let _ = child.wait().await;
        }
        self.complete = true;
    }
}

#[cfg(windows)]
impl Drop for JobChild {
    fn drop(&mut self) {
        if !self.complete {
            self.kill_group();
            if let Some(mut child) = self.child.take()
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}

impl BashTool {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            _workspace: workspace.into(),
        }
    }
}
impl Default for BashTool {
    fn default() -> Self {
        Self::new(std::env::current_dir().expect("current directory"))
    }
}

#[async_trait]
impl ToolProvider for BashTool {
    fn provider_id(&self) -> &'static str {
        "builtin.bash"
    }

    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: cookie_agent_protocol::ToolOutputDeclaration::Named {
                streams: vec!["stdout".into(), "stderr".into()],
            },
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: "bash".into(),
            permission_name: Self::get_permission_name("bash")?.into(),
            description: bash_tool_description().into(),
            parameters: schema::<BashArgs>(),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "bash" => Ok("bash"),
            _ => Err(ToolError::execution("bash provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: BashArgs = parse_args("bash", arguments.clone())?;
        if args.command.trim().is_empty() {
            return Err(ToolError::execution("command must not be empty"));
        }
        Ok((permission_name, Some(args.command)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, Some(command)) = self.get_permission_resource(name, arguments)? else {
            return Err(ToolError::execution("bash permission resource is missing"));
        };
        Ok(compact_command_line(&command))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let mut args: BashArgs = parse_args("bash", call.arguments)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::execution("command must not be empty"));
        }
        if args.timeout == 0 {
            args.timeout = default_timeout();
        }
        let executable_path = resolve_executable("bash")?;
        let executable = fs_cap::prepare_existing(Path::new("/"), &executable_path)?;
        if executable.directory || executable.identity.mode & 0o111 == 0 {
            return Err(ToolError::unsupported_security(
                "prepared bash executable is not an executable regular file",
            ));
        }
        let cwd = fs_cap::prepare_existing(std::path::Path::new("/"), &ctx.cwd)?;
        if !cwd.directory {
            return Err(ToolError::unsupported_security(
                "bash cwd is not a directory",
            ));
        }
        let mut executable_binding = executable.manifest_bytes()?;
        executable_binding.extend_from_slice(executable_path.as_os_str().as_encoded_bytes());
        let command = args.command.clone();
        let mut binding = command.as_bytes().to_vec();
        binding.extend_from_slice(&executable_binding);
        let resources = vec![prepared_resource(
            PermissionAction::Bash,
            "command",
            command.as_bytes(),
            &binding,
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::PrimaryOperation,
        )?];
        let policy_labels = vec![command.clone()];
        let mut context = cwd.manifest_bytes()?;
        context.extend_from_slice(&executable_binding);
        let operation = prepared_operation(
            "bash",
            &args,
            vec![(PermissionAction::Bash, "execute")],
            resources,
            &context,
        )?;
        let normalized_arguments = serde_json::json!({
            "command": command,
        });
        PreparedTool::new(
            operation,
            normalized_arguments,
            None,
            Box::new(BashExecutor {
                tool_call_id: call.id,
                args,
                cwd,
                executable,
            }),
        )?
        .with_policy_labels(policy_labels)
    }
}

#[cfg(unix)]
fn bash_tool_description() -> &'static str {
    "Execute one prepared shell command."
}

#[cfg(windows)]
fn bash_tool_description() -> &'static str {
    "Execute one prepared Git Bash command. Single-quote native Windows paths (for example, 'C:\\Users\\name\\file') or use C:/ paths."
}

fn compact_command_line(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl BashExecutor {
    #[cfg(unix)]
    async fn execute_process(
        self,
        progress: ProgressSink,
        cancellation: CancellationToken,
        stdin: Option<ToolStdin>,
    ) -> Result<ToolResult, ToolError> {
        self.cwd.revalidate()?;
        self.executable.revalidate()?;
        if cancellation.is_cancelled() {
            return Err(ToolError::execution("prepared bash cancelled"));
        }
        let mut command = Command::new(self.executable.proc_fd_path());
        command
            .arg("-c")
            .arg(&self.args.command)
            .current_dir(self.cwd.proc_fd_path())
            .stdin(if self.args.interactive {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let process_group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| ToolError::execution("prepared bash child has no process id"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::execution("bash stdout pipe missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::execution("bash stderr pipe missing"))?;
        let merged = Arc::new(Mutex::new(MergedOutput::default()));
        let stdout_task = tokio::spawn(read_output(
            stdout,
            OutputStream::Stdout,
            progress.clone(),
            self.tool_call_id,
            merged.clone(),
        ));
        let stderr_task = tokio::spawn(read_output(
            stderr,
            OutputStream::Stderr,
            progress,
            self.tool_call_id,
            merged.clone(),
        ));
        let stdin_task = if self.args.interactive {
            let mut child_stdin = child
                .stdin
                .take()
                .ok_or_else(|| ToolError::execution("bash stdin pipe missing"))?;
            let mut writes = stdin
                .ok_or_else(|| ToolError::execution("interactive bash stdin channel missing"))?;
            Some(tokio::spawn(async move {
                while let Some(write) = writes.recv().await {
                    child_stdin.write_all(&write.data).await?;
                    child_stdin.flush().await?;
                    if write.eof {
                        child_stdin.shutdown().await?;
                        break;
                    }
                }
                Ok::<(), std::io::Error>(())
            }))
        } else {
            None
        };
        let mut grouped = ProcessGroupChild {
            child: Some(child),
            process_group,
            complete: false,
        };
        enum WaitOutcome {
            Finished(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }
        let outcome = {
            let wait = grouped
                .child
                .as_mut()
                .expect("prepared child exists")
                .wait();
            tokio::pin!(wait);
            tokio::select! {
                result = tokio::time::timeout(Duration::from_millis(self.args.timeout), &mut wait) => {
                    match result {
                        Ok(result) => WaitOutcome::Finished(result),
                        Err(_) => WaitOutcome::TimedOut,
                    }
                }
                _ = cancellation.cancelled() => WaitOutcome::Cancelled,
            }
        };
        let status = match outcome {
            WaitOutcome::Finished(result) => {
                grouped.complete = true;
                result.map_err(|error| ToolError::execution(error.to_string()))?
            }
            WaitOutcome::TimedOut => {
                grouped.kill_and_reap().await;
                if let Some(task) = &stdin_task {
                    task.abort();
                }
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ToolError::execution("bash timed out"));
            }
            WaitOutcome::Cancelled => {
                grouped.kill_and_reap().await;
                if let Some(task) = &stdin_task {
                    task.abort();
                }
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ToolError::execution("prepared bash cancelled"));
            }
        };
        if let Some(task) = &stdin_task {
            task.abort();
        }
        grouped.kill_group();
        stdout_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))??;
        stderr_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))??;
        if status.code().is_none() {
            return Err(ToolError::execution("bash terminated by a signal"));
        }
        Ok(ToolResult {
            display: Some(completed_display(
                std::mem::take(&mut *merged.lock().await),
                status.code(),
            )),
            retained_output: None,
            title: crate::safe_title("Bash"),
            output: String::new(),
            metadata: serde_json::json!({"status":status.code(),"success":status.success()}),
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
    }

    #[cfg(windows)]
    async fn execute_process(
        self,
        progress: ProgressSink,
        cancellation: CancellationToken,
        stdin: Option<ToolStdin>,
    ) -> Result<ToolResult, ToolError> {
        self.cwd.revalidate()?;
        self.executable.revalidate()?;
        if cancellation.is_cancelled() {
            return Err(ToolError::execution("prepared bash cancelled"));
        }
        let mut command = Command::new(self.executable.proc_fd_path());
        command
            .arg("-c")
            .arg(&self.args.command)
            .current_dir(self.cwd.proc_fd_path())
            .stdin(if self.args.interactive {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(ProcessJobObject).wrap(KillOnDrop);
        // ProcessJobObject adds CREATE_SUSPENDED, assigns the process to the
        // job, and only then resumes its threads, so descendants cannot escape.
        let mut child = wrapped
            .spawn()
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| ToolError::execution("bash stdout pipe missing"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| ToolError::execution("bash stderr pipe missing"))?;
        let merged = Arc::new(Mutex::new(MergedOutput::default()));
        let stdout_task = tokio::spawn(read_output(
            stdout,
            OutputStream::Stdout,
            progress.clone(),
            self.tool_call_id,
            merged.clone(),
        ));
        let stderr_task = tokio::spawn(read_output(
            stderr,
            OutputStream::Stderr,
            progress,
            self.tool_call_id,
            merged.clone(),
        ));
        let stdin_task = if self.args.interactive {
            let mut child_stdin = child
                .stdin()
                .take()
                .ok_or_else(|| ToolError::execution("bash stdin pipe missing"))?;
            let mut writes = stdin
                .ok_or_else(|| ToolError::execution("interactive bash stdin channel missing"))?;
            Some(tokio::spawn(async move {
                while let Some(write) = writes.recv().await {
                    child_stdin.write_all(&write.data).await?;
                    child_stdin.flush().await?;
                    if write.eof {
                        child_stdin.shutdown().await?;
                        break;
                    }
                }
                Ok::<(), std::io::Error>(())
            }))
        } else {
            None
        };
        let mut grouped = JobChild {
            child: Some(child),
            complete: false,
        };
        enum WaitOutcome {
            Finished(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }
        let outcome = {
            let wrapped_child = grouped.child.as_mut().expect("prepared child exists");
            // SAFETY: only the raw parent's wait state is observed. The wrapper
            // remains owned by JobChild and retains all job/kill state and pipes.
            let wait = unsafe { wrapped_child.inner_child_mut() }.wait();
            tokio::pin!(wait);
            tokio::select! {
                result = tokio::time::timeout(Duration::from_millis(self.args.timeout), &mut wait) => {
                    match result {
                        Ok(result) => WaitOutcome::Finished(result),
                        Err(_) => WaitOutcome::TimedOut,
                    }
                }
                _ = cancellation.cancelled() => WaitOutcome::Cancelled,
            }
        };
        let status = match outcome {
            WaitOutcome::Finished(result) => {
                result.map_err(|error| ToolError::execution(error.to_string()))?
            }
            WaitOutcome::TimedOut => {
                grouped.kill_and_reap().await;
                if let Some(task) = &stdin_task {
                    task.abort();
                }
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ToolError::execution("bash timed out"));
            }
            WaitOutcome::Cancelled => {
                grouped.kill_and_reap().await;
                if let Some(task) = &stdin_task {
                    task.abort();
                }
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ToolError::execution("prepared bash cancelled"));
            }
        };
        if let Some(task) = &stdin_task {
            task.abort();
        }
        grouped.kill_and_reap().await;
        stdout_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))??;
        stderr_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))??;
        Ok(ToolResult {
            display: Some(completed_display(
                std::mem::take(&mut *merged.lock().await),
                status.code(),
            )),
            retained_output: None,
            title: crate::safe_title("Bash"),
            output: String::new(),
            metadata: serde_json::json!({"status":status.code(),"success":status.success()}),
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
    }
}

#[async_trait]
impl PreparedExecutor for BashExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        self.cwd.revalidate()?;
        self.executable.revalidate()
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_engine::ToolCompletion, ToolError> {
        let result: Result<ToolResult, ToolError> = async move {
            self.execute_process(context.progress, context.cancellation, context.stdin)
                .await
        }
        .await;
        result.map(cookie_agent_engine::ToolCompletion::streamed)
    }
}

fn completed_display(merged: MergedOutput, status: Option<i32>) -> String {
    let mut output = merged.text;
    let suffix = if status != Some(0) {
        format!(
            "\nExit status: {}",
            status.map_or_else(|| "unknown".into(), |code| code.to_string())
        )
    } else {
        String::new()
    };
    const MARKER: &str = "\n[display truncated]";
    let maximum = cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES - suffix.len();
    if merged.truncated || output.len() > maximum {
        let mut end = output.len().min(maximum - MARKER.len());
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str(MARKER);
    }
    output.push_str(&suffix);
    output
}

#[cfg(unix)]
fn resolve_executable(name: &str) -> Result<PathBuf, ToolError> {
    let path =
        env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    resolve_executable_in_path(name, &path)
}

#[cfg(unix)]
fn resolve_executable_in_path(name: &str, path: &std::ffi::OsStr) -> Result<PathBuf, ToolError> {
    for directory in env::split_paths(path) {
        let candidate = directory.join(name);
        let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if executable_metadata_is_supported(&metadata) {
            return candidate
                .canonicalize()
                .map_err(|error| ToolError::execution(error.to_string()));
        }
    }
    Err(ToolError::execution(format!(
        "unable to resolve executable `{name}` from PATH during preparation"
    )))
}

#[cfg(unix)]
fn executable_metadata_is_supported(metadata: &std::fs::Metadata) -> bool {
    !metadata.file_type().is_symlink() && metadata.is_file() && metadata.mode() & 0o111 != 0
}

#[cfg(windows)]
fn resolve_executable(name: &str) -> Result<PathBuf, ToolError> {
    static GIT_BASH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

    if name != "bash" {
        return Err(ToolError::execution(format!(
            "Windows executable discovery does not support `{name}`"
        )));
    }
    GIT_BASH
        .get_or_init(discover_git_bash)
        .clone()
        .map_err(ToolError::execution)
}

#[cfg(windows)]
fn discover_git_bash() -> Result<PathBuf, String> {
    for candidate in where_executable("bash") {
        if let Some(shell) = validate_git_bash(&candidate) {
            return Ok(shell);
        }
    }

    for variable in ["EXEPATH", "MSYSTEM"] {
        let Some(value) = env::var_os(variable) else {
            continue;
        };
        for hint in env::split_paths(&value) {
            for candidate in git_bash_candidates_from_hint(&hint) {
                if let Some(shell) = validate_git_bash(&candidate) {
                    return Ok(shell);
                }
            }
        }
    }

    for candidate in [
        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
    ] {
        if let Some(shell) = validate_git_bash(&candidate) {
            return Ok(shell);
        }
    }

    for git in where_executable("git") {
        for candidate in git_bash_candidates_from_git(&git) {
            if let Some(shell) = validate_git_bash(&candidate) {
                return Ok(shell);
            }
        }
    }

    Err("git-bash not found; install Git for Windows and ensure its bin directory or git.exe is on PATH"
        .to_owned())
}

#[cfg(windows)]
fn where_executable(name: &str) -> Vec<PathBuf> {
    let Ok(output) = std::process::Command::new("where.exe").arg(name).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(windows)]
fn git_bash_candidates_from_hint(hint: &Path) -> Vec<PathBuf> {
    let mut roots = vec![hint.to_owned()];
    if hint
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("cmd"))
        && let Some(parent) = hint.parent()
    {
        roots.push(parent.to_owned());
    }
    roots
        .into_iter()
        .flat_map(|root| {
            [
                root.join("bash.exe"),
                root.join("bin").join("bash.exe"),
                root.join("usr").join("bin").join("bash.exe"),
            ]
        })
        .collect()
}

#[cfg(windows)]
fn git_bash_candidates_from_git(git: &Path) -> Vec<PathBuf> {
    let Some(directory) = git.parent() else {
        return Vec::new();
    };
    let mut roots = vec![directory.to_owned()];
    if directory
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("shims"))
        && let Some(scoop) = directory.parent()
    {
        roots.push(scoop.join("apps").join("git").join("current"));
    }
    if let Some(parent) = directory.parent() {
        roots.push(parent.to_owned());
        if directory
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
            && let Some(grandparent) = parent.parent()
        {
            roots.push(grandparent.to_owned());
        }
    }
    roots
        .into_iter()
        .flat_map(|root| {
            [
                root.join("bin").join("bash.exe"),
                root.join("usr").join("bin").join("bash.exe"),
            ]
        })
        .collect()
}

#[cfg(windows)]
fn validate_git_bash(candidate: &Path) -> Option<PathBuf> {
    let canonical = candidate.canonicalize().ok()?;
    if canonical
        .file_name()
        .is_none_or(|name| !name.eq_ignore_ascii_case("bash.exe"))
        || is_system32_bash(&canonical)
        || !canonical.metadata().ok()?.is_file()
    {
        return None;
    }

    let bin = canonical.parent()?;
    let root = if bin
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("usr"))
    {
        bin.parent()?.parent()?
    } else {
        bin.parent()?
    };
    [
        root.join("cmd").join("git.exe"),
        root.join("bin").join("git.exe"),
        root.join("mingw64").join("bin").join("git.exe"),
        root.join("mingw32").join("bin").join("git.exe"),
    ]
    .iter()
    .any(|git| git.is_file())
    .then_some(canonical)
}

#[cfg(windows)]
fn is_system32_bash(path: &Path) -> bool {
    is_system32_bash_with_identity(path, canonical_system32_bash())
}

#[cfg(windows)]
fn canonical_system32_bash() -> Option<&'static Path> {
    static SYSTEM32_BASH: OnceLock<Option<PathBuf>> = OnceLock::new();

    SYSTEM32_BASH
        .get_or_init(|| canonical_system32_bash_from_windows_directory(&windows_directory()?))
        .as_deref()
}

#[cfg(windows)]
fn windows_directory() -> Option<PathBuf> {
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    let mut buffer = vec![0_u16; 260];
    loop {
        let length =
            unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), u32::try_from(buffer.len()).ok()?) };
        if length == 0 {
            break;
        }
        let length = usize::try_from(length).ok()?;
        if length < buffer.len() {
            return Some(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        if length > 32_767 {
            break;
        }
        buffer.resize(length + 1, 0);
    }

    select_windows_directory(
        None,
        env::var_os("SYSTEMROOT").map(PathBuf::from),
        env::var_os("WINDIR").map(PathBuf::from),
    )
}

#[cfg(windows)]
fn select_windows_directory(
    api: Option<PathBuf>,
    system_root: Option<PathBuf>,
    windir: Option<PathBuf>,
) -> Option<PathBuf> {
    api.or(system_root).or(windir)
}

#[cfg(windows)]
fn canonical_system32_bash_from_windows_directory(windows: &Path) -> Option<PathBuf> {
    windows
        .join("System32")
        .canonicalize()
        .ok()
        .map(|system32| system32.join("bash.exe"))
}

#[cfg(windows)]
fn is_system32_bash_with_identity(path: &Path, system32_bash: Option<&Path>) -> bool {
    is_system32_shaped_bash(path)
        || system32_bash.is_some_and(|system32| windows_paths_eq(path, system32))
}

#[cfg(windows)]
fn is_system32_shaped_bash(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bash.exe"))
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.eq_ignore_ascii_case("System32"))
}

#[cfg(windows)]
fn windows_paths_eq(left: &Path, right: &Path) -> bool {
    use std::path::{Component, Prefix};

    fn drive_letter(prefix: Prefix<'_>) -> Option<u8> {
        match prefix {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
            _ => None,
        }
    }

    fn components_eq(left: Component<'_>, right: Component<'_>) -> bool {
        match (left, right) {
            (Component::Prefix(left), Component::Prefix(right)) => {
                match (drive_letter(left.kind()), drive_letter(right.kind())) {
                    (Some(left), Some(right)) => left.eq_ignore_ascii_case(&right),
                    _ => left.as_os_str().eq_ignore_ascii_case(right.as_os_str()),
                }
            }
            (left, right) => left.as_os_str().eq_ignore_ascii_case(right.as_os_str()),
        }
    }

    let mut left = left.components();
    let mut right = right.components();
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) if components_eq(left, right) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        Storage::FileSystem::GetShortPathNameW,
        System::Threading::{OpenProcess, WaitForSingleObject},
    };

    use super::{
        CommandWrap, KillOnDrop, ProcessJobObject, canonical_system32_bash,
        canonical_system32_bash_from_windows_directory, git_bash_candidates_from_git,
        is_system32_bash, is_system32_bash_with_identity, resolve_executable,
        select_windows_directory, windows_paths_eq,
    };

    const TEST_NAME: &str = "bash::windows_tests::job_object_kills_spawned_process_tree";

    fn single_quote_for_bash(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn short_path(path: &Path) -> Option<PathBuf> {
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let required = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
        if required == 0 {
            return None;
        }
        let mut output = vec![0_u16; required as usize];
        let written = unsafe { GetShortPathNameW(wide.as_ptr(), output.as_mut_ptr(), required) };
        (written != 0 && written < required)
            .then(|| PathBuf::from(OsString::from_wide(&output[..written as usize])))
    }

    #[test]
    fn discovery_rejects_wsl_and_covers_scoop_git_shims() {
        let system32 = Path::new(r"C:\Windows\System32\bash.exe");
        assert!(windows_paths_eq(
            Path::new(r"\\?\C:\Windows\System32\bash.exe"),
            system32,
        ));
        assert!(windows_paths_eq(
            Path::new(r"c:\WINDOWS\system32\BASH.EXE"),
            system32,
        ));
        assert!(!windows_paths_eq(
            Path::new(r"C:\Windows\System32-evil\bash.exe"),
            system32,
        ));

        let candidates =
            git_bash_candidates_from_git(Path::new(r"C:\Users\name\scoop\shims\git.exe"));
        assert!(candidates.iter().any(|candidate| {
            candidate
                .to_string_lossy()
                .eq_ignore_ascii_case(r"C:\Users\name\scoop\apps\git\current\bin\bash.exe")
        }));
    }

    #[test]
    fn system32_guard_fails_closed_without_trusted_identity() {
        for candidate in [
            r"C:\Windows\System32\bash.exe",
            r"\\?\C:\Windows\System32\bash.exe",
            r"c:\WINDOWS\system32\BASH.EXE",
        ] {
            assert!(is_system32_bash_with_identity(Path::new(candidate), None));
        }
        assert!(!is_system32_bash_with_identity(
            Path::new(r"C:\Windows\System32-evil\bash.exe"),
            None,
        ));
    }

    #[test]
    fn system32_guard_rejects_shape_with_mismatched_identity() {
        let fake_identity = Path::new(r"\\?\D:\FakeWindows\System32\bash.exe");
        assert!(is_system32_bash_with_identity(
            Path::new(r"C:\Windows\System32\bash.exe"),
            Some(fake_identity),
        ));
        assert!(!is_system32_bash_with_identity(
            Path::new(r"C:\Windows\System32-evil\bash.exe"),
            Some(fake_identity),
        ));
    }

    #[test]
    fn windows_directory_resolution_handles_missing_and_unusable_windir() {
        let api = PathBuf::from(r"C:\Windows");
        assert_eq!(
            select_windows_directory(Some(api.clone()), None, None),
            Some(api.clone())
        );
        assert_eq!(
            select_windows_directory(None, Some(api.clone()), None),
            Some(api)
        );
        assert_eq!(select_windows_directory(None, None, None), None);

        let directory = tempfile::tempdir().expect("temporary root");
        let unusable_windir = directory.path().join("missing-windows");
        let selected = select_windows_directory(None, None, Some(unusable_windir))
            .expect("WINDIR fallback selected");
        let identity = canonical_system32_bash_from_windows_directory(&selected);
        assert_eq!(identity, None);
        assert!(is_system32_bash_with_identity(
            Path::new(r"C:\Windows\System32\bash.exe"),
            identity.as_deref(),
        ));
    }

    #[test]
    fn system32_short_name_is_rejected_when_available() {
        let Some(system32) = canonical_system32_bash() else {
            return;
        };
        let Some(short) = short_path(system32) else {
            return;
        };
        if !windows_paths_eq(&short, system32) {
            let canonical_short = short.canonicalize().expect("canonicalize 8.3 alias");
            assert!(is_system32_bash(&canonical_short));
        }
    }

    #[test]
    fn discovered_git_bash_reads_native_windows_paths() {
        let shell = resolve_executable("bash").expect("Git Bash on windows-latest");
        let directory = tempfile::tempdir().expect("temporary root");
        let file = directory.path().join("native path.txt");
        std::fs::write(&file, b"native-path-round-trip").expect("write fixture");

        let native = file.to_string_lossy();
        let slash_form = native.replace('\\', "/");
        for path in [native.as_ref(), slash_form.as_str()] {
            let output = Command::new(&shell)
                .args(["-c", &format!("cat -- {}", single_quote_for_bash(path))])
                .output()
                .expect("invoke discovered Git Bash");
            assert!(
                output.status.success(),
                "Git Bash could not read {path:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"native-path-round-trip");
        }
    }

    #[tokio::test]
    async fn job_object_kills_spawned_process_tree() {
        if std::env::var_os("COOKIE_JOB_LEAF").is_some() {
            tokio::time::sleep(Duration::from_secs(30)).await;
            return;
        }
        if let Some(pid_file) = std::env::var_os("COOKIE_JOB_PARENT") {
            let mut leaves = Vec::new();
            for _ in 0..8 {
                leaves.push(
                    tokio::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", TEST_NAME, "--nocapture"])
                        .env("COOKIE_JOB_LEAF", "1")
                        .spawn()
                        .expect("spawn leaf"),
                );
            }
            std::fs::write(
                pid_file,
                leaves
                    .iter()
                    .map(|leaf| leaf.id().expect("leaf pid").to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            )
            .expect("pid file");
            drop(leaves);
            return;
        }

        let directory = tempfile::tempdir().expect("temporary root");
        let pid_file = directory.path().join("leaf.pid");
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env("COOKIE_JOB_PARENT", &pid_file);
        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(ProcessJobObject).wrap(KillOnDrop);
        let mut parent = wrapped.spawn().expect("spawn suspended job parent");
        let leaf_pids = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // The writer's `File::create` is observable before its
                // `write_all`; retry until the pid list is fully present.
                let ready = std::fs::read_to_string(&pid_file).ok().and_then(|pids| {
                    pids.split(',')
                        .map(|pid| pid.parse::<u32>().ok())
                        .collect::<Option<Vec<_>>>()
                });
                if let Some(pids) = ready {
                    break pids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leaves started");
        let parent_status = tokio::time::timeout(Duration::from_secs(2), async {
            // SAFETY: the wrapper remains alive and only parent wait state changes.
            unsafe { parent.inner_child_mut() }.wait().await
        })
        .await
        .expect("parent prompt return was delayed by descendants")
        .expect("parent wait");
        assert!(parent_status.success());
        parent.start_kill().expect("terminate job");
        tokio::time::timeout(Duration::from_secs(5), parent.wait())
            .await
            .expect("parent terminated")
            .expect("parent wait");

        const SYNCHRONIZE: u32 = 0x0010_0000;
        for leaf_pid in leaf_pids {
            let leaf = unsafe { OpenProcess(SYNCHRONIZE, 0, leaf_pid) };
            if !leaf.is_null() {
                let waited = unsafe { WaitForSingleObject(leaf, 5_000) };
                unsafe {
                    CloseHandle(leaf);
                }
                assert_eq!(waited, WAIT_OBJECT_0, "leaf survived job termination");
            }
        }
    }
}
