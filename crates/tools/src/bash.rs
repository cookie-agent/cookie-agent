use std::{
    env,
    ffi::OsString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProgress, ToolProvider, ToolSpec, ToolStdin,
};
use cookie_agent_protocol::PersistedToolResult as ToolResult;
use cookie_agent_protocol::{
    ApprovalResourceSource, OutputStream, PermissionAction, PreparedBindingLifetime,
    SafeDisplayText, ToolCallId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
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
pub const OUTPUT_CHUNK_CUMULATIVE_CAP: usize = 1024 * 1024;
const OUTPUT_CHUNK_TRUNCATED_MESSAGE: &str =
    "Live bash output truncated after 1 MiB; the terminal result remains authoritative";

#[derive(Debug, Default)]
struct OutputPreviewState {
    emitted: usize,
    stopped: bool,
}

#[derive(Debug, Default)]
struct OutputPreviewBudget {
    state: Mutex<OutputPreviewState>,
}

impl OutputPreviewBudget {
    fn retain(&self, chunk: &str) -> (Option<String>, bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.stopped {
            return (None, false);
        }
        let remaining = OUTPUT_CHUNK_CUMULATIVE_CAP.saturating_sub(state.emitted);
        if remaining == 0 {
            state.stopped = true;
            return (None, !chunk.is_empty());
        }
        let end = chunk
            .char_indices()
            .take_while(|(index, character)| index + character.len_utf8() <= remaining)
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0);
        let retained = &chunk[..end];
        state.emitted += retained.len();
        state.stopped = end < chunk.len();
        (
            (!retained.is_empty()).then(|| retained.to_owned()),
            state.stopped,
        )
    }
}

fn sanitized_chunks(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for character in text.chars() {
        let character = if character.is_control() {
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
    stream: OutputStream,
    bytes: &[u8],
    budget: &OutputPreviewBudget,
    truncation_emitted: &std::sync::atomic::AtomicBool,
) -> Result<(), ToolError> {
    let stream_name = match stream {
        OutputStream::Stdout => "stdout",
        OutputStream::Stderr => "stderr",
    };
    for chunk in sanitized_chunks(bytes) {
        let (retained, truncated) = budget.retain(&chunk);
        if let Some(output_chunk) = retained {
            progress
                .send(ToolProgress {
                    tool_call_id,
                    message: format!("bash {stream_name}"),
                    output_chunk: Some(output_chunk),
                })
                .await?;
        }
        if truncated && !truncation_emitted.swap(true, std::sync::atomic::Ordering::AcqRel) {
            progress
                .send(ToolProgress {
                    tool_call_id,
                    message: OUTPUT_CHUNK_TRUNCATED_MESSAGE.into(),
                    output_chunk: None,
                })
                .await?;
        }
    }
    Ok(())
}

async fn read_output<R>(
    mut reader: R,
    stream: OutputStream,
    progress: ProgressSink,
    tool_call_id: ToolCallId,
    budget: Arc<OutputPreviewBudget>,
    truncation_emitted: Arc<std::sync::atomic::AtomicBool>,
) -> Result<Vec<u8>, ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut read_buffer = [0_u8; OUTPUT_CHUNK_FLUSH_BYTES];
    let flush = tokio::time::sleep(OUTPUT_CHUNK_FLUSH_INTERVAL);
    tokio::pin!(flush);
    loop {
        tokio::select! {
            read = reader.read(&mut read_buffer) => {
                let count = read.map_err(|error| ToolError::execution(error.to_string()))?;
                if count == 0 {
                    if !pending.is_empty() {
                        emit_preview(
                            &progress,
                            tool_call_id,
                            stream,
                            &pending,
                            &budget,
                            &truncation_emitted,
                        ).await?;
                    }
                    return Ok(output);
                }
                let bytes = &read_buffer[..count];
                progress.output(stream, bytes);
                output.extend_from_slice(bytes);
                if pending.is_empty() {
                    flush.as_mut().reset(tokio::time::Instant::now() + OUTPUT_CHUNK_FLUSH_INTERVAL);
                }
                pending.extend_from_slice(bytes);
                if pending.len() >= OUTPUT_CHUNK_FLUSH_BYTES {
                    emit_preview(
                        &progress,
                        tool_call_id,
                        stream,
                        &pending,
                        &budget,
                        &truncation_emitted,
                    ).await?;
                    pending.clear();
                }
            }
            () = &mut flush, if !pending.is_empty() => {
                emit_preview(
                    &progress,
                    tool_call_id,
                    stream,
                    &pending,
                    &budget,
                    &truncation_emitted,
                ).await?;
                pending.clear();
                flush.as_mut().reset(tokio::time::Instant::now() + OUTPUT_CHUNK_FLUSH_INTERVAL);
            }
        }
    }
}

struct ProcessGroupChild {
    child: Option<Child>,
    process_group: i32,
    complete: bool,
}

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
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "bash".into(),
            permission_name: Self::get_permission_name("bash")?.into(),
            description: "Execute one prepared shell command.".into(),
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

fn compact_command_line(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl BashExecutor {
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
            .arg("-lc")
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
        let budget = Arc::new(OutputPreviewBudget::default());
        let truncation_emitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stdout_task = tokio::spawn(read_output(
            stdout,
            OutputStream::Stdout,
            progress.clone(),
            self.tool_call_id,
            Arc::clone(&budget),
            Arc::clone(&truncation_emitted),
        ));
        let stderr_task = tokio::spawn(read_output(
            stderr,
            OutputStream::Stderr,
            progress,
            self.tool_call_id,
            budget,
            truncation_emitted,
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
        let stdout = stdout_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))??;
        let stderr = stderr_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))??;
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        Ok(ToolResult {
            title: crate::safe_title("Bash"),
            output: format!("{stdout}{stderr}"),
            metadata: serde_json::json!({"status":status.code(),"success":status.success()}),
            truncation: None,
            attachments: Vec::new(),
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
    ) -> Result<ToolResult, ToolError> {
        self.execute_process(context.progress, context.cancellation, context.stdin)
            .await
    }
}

fn resolve_executable(name: &str) -> Result<PathBuf, ToolError> {
    let path =
        env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    resolve_executable_in_path(name, &path)
}

fn resolve_executable_in_path(name: &str, path: &std::ffi::OsStr) -> Result<PathBuf, ToolError> {
    for directory in env::split_paths(path) {
        let candidate = directory.join(name);
        let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if !metadata.file_type().is_symlink() && metadata.is_file() && metadata.mode() & 0o111 != 0
        {
            return candidate
                .canonicalize()
                .map_err(|error| ToolError::execution(error.to_string()));
        }
    }
    Err(ToolError::execution(format!(
        "unable to resolve executable `{name}` from PATH during preparation"
    )))
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use cookie_agent_engine::permissions::PermissionPipeline;
    use cookie_agent_engine::{
        ProgressSink, ToolCall, ToolError, ToolPreparationContext, ToolProvider, events::OutputHub,
    };
    use cookie_agent_protocol::{
        AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot, OutputStream,
        PermissionAction, PermissionEffect, PermissionRule, RunId, SafeDisplayText, SessionId,
        Sha256Digest, ToolCallId, WildcardPattern,
    };
    use tokio::io::AsyncWriteExt;

    use super::{
        BashArgs, BashExecutor, BashTool, OUTPUT_CHUNK_CUMULATIVE_CAP, OutputPreviewBudget,
        read_output, resolve_executable, resolve_executable_in_path,
    };

    #[test]
    fn permission_resource_is_the_command() {
        let tool = BashTool::new("/tmp");
        assert_eq!(
            tool.get_permission_resource("bash", &serde_json::json!({"command":"git status"}))
                .expect("permission resource"),
            ("bash", Some("git status".into()))
        );
        assert!(matches!(
            tool.get_permission_resource("bash", &serde_json::json!({"command":"   "})),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn display_argument_is_a_one_line_command_that_keeps_and_segments() {
        let tool = BashTool::new("/tmp");
        assert_eq!(
            tool.get_display_argument(
                "bash",
                &serde_json::json!({"command":"git status && cargo test"})
            )
            .expect("compound"),
            "git status && cargo test"
        );
        assert_eq!(
            tool.get_display_argument(
                "bash",
                &serde_json::json!({"command":"git\n  status &&\ncargo test"})
            )
            .expect("multiline"),
            "git status && cargo test"
        );
        assert!(matches!(
            tool.get_display_argument("bash", &serde_json::json!({"command":"   "})),
            Err(ToolError::Failed(_))
        ));
    }

    async fn prepare(root: &Path, command: &str) -> cookie_agent_engine::PreparedTool {
        BashTool::new(root)
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.to_owned(),
                    workspace_root: root.to_owned(),
                    turn_context: crate::test_turn_context(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":command}),
                },
            )
            .await
            .expect("prepare")
    }

    fn policy(rules: Vec<PermissionRule>) -> AgentSnapshot {
        AgentSnapshot {
            agent: AgentId::new("test").expect("agent id"),
            schema: AgentSchemaVersion::current(),
            mode: AgentMode::Primary,
            description: "Test agent".into(),
            document_source: AgentDocumentSource::Workspace,
            document_fingerprint: Sha256Digest::of_bytes(b"test document"),
            composed_prompt: "Test permission evaluation.\n".into(),
            prompt_fingerprint: Sha256Digest::of_bytes(b"Test permission evaluation.\n"),
            max_output_tokens: 0,
            permissions: rules,
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        }
    }

    fn rule(action: PermissionAction, resource: &str, effect: PermissionEffect) -> PermissionRule {
        PermissionRule {
            action,
            resource: WildcardPattern::new(resource).expect("wildcard"),
            effect,
        }
    }

    #[tokio::test]
    async fn whole_command_label_is_one_resource() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "echo one; echo one").await;
        assert_eq!(
            prepared.policy_labels(),
            [Some("echo one; echo one".into())]
        );
        assert_eq!(
            prepared.operation().resources()[0].capability,
            PermissionAction::Bash
        );
        assert_eq!(
            prepared.normalized_arguments(),
            &serde_json::json!({"command":"echo one; echo one"})
        );
    }

    #[tokio::test]
    async fn bash_never_produces_read_or_write_resources() {
        let root = tempfile::tempdir().expect("root");
        for command in [
            "cat .env",
            "ls ordinary/",
            "rm -rf build/",
            "ls | tee out.txt",
        ] {
            let prepared = prepare(root.path(), command).await;
            assert_eq!(
                prepared.operation().resources()[0].capability,
                PermissionAction::Bash
            );
        }
        let env = prepare(root.path(), "cat .env").await;
        assert_eq!(env.policy_labels(), [Some("cat .env".into())]);
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![
                rule(PermissionAction::Bash, "*", PermissionEffect::Allow),
                rule(PermissionAction::Bash, "cat *", PermissionEffect::Deny),
            ]),
            env.operation(),
            env.policy_labels(),
            root.path(),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
    }

    #[tokio::test]
    async fn compound_command_is_one_whole_command_resource() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "git status && rm -rf x").await;
        assert_eq!(
            prepared.policy_labels(),
            [Some("git status && rm -rf x".into())]
        );
        assert_eq!(
            prepared.normalized_arguments(),
            &serde_json::json!({"command":"git status && rm -rf x"})
        );
    }

    #[tokio::test]
    async fn git_star_matches_a_compound_command_by_wildcard_rules() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "git status && rm -rf x").await;
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![rule(
                PermissionAction::Bash,
                "git *",
                PermissionEffect::Allow,
            )]),
            prepared.operation(),
            prepared.policy_labels(),
            root.path(),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[tokio::test]
    async fn prefix_rm_star_does_not_match_a_compound_git_command() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "git status && rm -rf x").await;
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![
                rule(PermissionAction::Bash, "*", PermissionEffect::Allow),
                rule(PermissionAction::Bash, "rm *", PermissionEffect::Deny),
            ]),
            prepared.operation(),
            prepared.policy_labels(),
            root.path(),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[tokio::test]
    async fn containment_rm_denies_a_compound_command() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "git status && rm -rf x").await;
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![
                rule(PermissionAction::Bash, "*", PermissionEffect::Allow),
                rule(PermissionAction::Bash, "*rm*", PermissionEffect::Deny),
            ]),
            prepared.operation(),
            prepared.policy_labels(),
            root.path(),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
    }

    #[tokio::test]
    async fn simple_command_is_matched_as_itself() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "pwd").await;
        assert_eq!(prepared.policy_labels(), [Some("pwd".into())]);
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![rule(
                PermissionAction::Bash,
                "pwd",
                PermissionEffect::Allow,
            )]),
            prepared.operation(),
            prepared.policy_labels(),
            root.path(),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[tokio::test]
    async fn complex_shell_constructs_keep_every_resource_on_bash() {
        let root = tempfile::tempdir().expect("root");
        for command in ["git status", "ls && rm x", "ls > out", "cat $(f)"] {
            let prepared = prepare(root.path(), command).await;
            assert_eq!(
                prepared.operation().resources()[0].capability,
                PermissionAction::Bash
            );
        }
    }

    #[test]
    fn fake_path_swap_cannot_change_prepared_executable() {
        let root = tempfile::tempdir().expect("root");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let executable = bin.join("bash");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("mode");
        let path = resolve_executable_in_path("bash", bin.as_os_str()).expect("resolve");
        let prepared = crate::fs_cap::prepare_existing(Path::new("/"), &path).expect("prepare");
        fs::rename(&executable, bin.join("old-bash")).expect("swap old");
        fs::write(&executable, "#!/bin/sh\nexit 42\n").expect("replacement");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(matches!(
            prepared.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));
    }

    #[tokio::test]
    async fn interactive_is_prepared_for_runtime_stdin() {
        let root = tempfile::tempdir().expect("root");
        let tool = BashTool::new(root.path());
        let result = tool
            .prepare(
                cookie_agent_engine::ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.path().to_owned(),
                    workspace_root: root.path().to_owned(),
                    turn_context: crate::test_turn_context(),
                },
                cookie_agent_engine::ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "bash".into(),
                    arguments: serde_json::json!({
                        "command": "echo unsafe",
                        "interactive": true
                    }),
                },
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn output_reader_streams_bounded_sanitized_chunks_and_retains_full_output() {
        let call_id = ToolCallId::new_v7();
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(2_048);
        let progress = ProgressSink::new(progress_tx, OutputHub::new(call_id, 64 * 1024));
        let (mut writer, reader) = tokio::io::duplex(2 * 1024 * 1024);
        let read = tokio::spawn(read_output(
            reader,
            OutputStream::Stdout,
            progress,
            call_id,
            std::sync::Arc::new(OutputPreviewBudget::default()),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ));
        writer.write_all(b"first\n").await.expect("first output");
        let first = tokio::time::timeout(std::time::Duration::from_millis(250), progress_rx.recv())
            .await
            .expect("first chunk timeout")
            .expect("first chunk");
        assert_eq!(first.output_chunk.as_deref(), Some("first "));
        assert!(!read.is_finished());

        let overflow = vec![b'x'; OUTPUT_CHUNK_CUMULATIVE_CAP + 1];
        writer.write_all(&overflow).await.expect("overflow output");
        writer.shutdown().await.expect("output eof");
        drop(writer);

        let mut chunks = vec![first];
        while let Some(progress) = progress_rx.recv().await {
            chunks.push(progress);
        }
        let output = read.await.expect("reader task").expect("read complete");
        assert_eq!(&output[..6], b"first\n");
        assert_eq!(output.len(), overflow.len() + 6);
        assert!(chunks.iter().all(|progress| {
            progress
                .output_chunk
                .as_ref()
                .is_none_or(|chunk| chunk.len() <= SafeDisplayText::MAX_BYTES)
        }));
        assert_eq!(
            chunks
                .iter()
                .filter_map(|progress| progress.output_chunk.as_ref())
                .map(String::len)
                .sum::<usize>(),
            OUTPUT_CHUNK_CUMULATIVE_CAP
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|progress| progress.message.contains("terminal result"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn real_bash_timeout_drains_progress_before_terminal_completion() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let root = tempfile::tempdir().expect("root");
            let call_id = ToolCallId::new_v7();
            let executable_path = resolve_executable("bash").expect("bash executable");
            let executor = BashExecutor {
                tool_call_id: call_id,
                args: BashArgs {
                    command: "printf 'ready\\n'; sleep 10".into(),
                    timeout: 2_000,
                    interactive: false,
                },
                cwd: crate::fs_cap::prepare_existing(Path::new("/"), root.path())
                    .expect("prepared cwd"),
                executable: crate::fs_cap::prepare_existing(Path::new("/"), &executable_path)
                    .expect("prepared executable"),
            };
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);
            let progress = ProgressSink::new(progress_tx, OutputHub::new(call_id, 64 * 1024));
            let execute = executor.execute_process(
                progress,
                tokio_util::sync::CancellationToken::new(),
                None,
            );
            tokio::pin!(execute);
            let mut event_order = Vec::new();
            let mut progress_open = true;
            let error = loop {
                tokio::select! {
                    progress = progress_rx.recv(), if progress_open => {
                        if let Some(progress) = progress {
                            if let Some(chunk) = progress.output_chunk {
                                event_order.push(("progress", chunk));
                            }
                        } else {
                            progress_open = false;
                        }
                    }
                    result = &mut execute => {
                        while let Ok(progress) = progress_rx.try_recv() {
                            if let Some(chunk) = progress.output_chunk {
                                event_order.push(("progress", chunk));
                            }
                        }
                        event_order.push(("terminal", String::new()));
                        break result.expect_err("bash must time out");
                    }
                }
            };

            assert!(error.to_string().contains("bash timed out"));
            assert_eq!(event_order.last().map(|event| event.0), Some("terminal"));
            assert!(
                event_order[..event_order.len() - 1]
                    .iter()
                    .any(|(kind, chunk)| *kind == "progress" && chunk.contains("ready"))
            );
        })
        .await
        .expect("bash timeout progress test exceeded 30 seconds");
    }

    #[tokio::test]
    async fn killing_prepared_process_group_removes_descendants() {
        let root = tempfile::tempdir().expect("root");
        let pid_file = root.path().join("pid");
        let shell = resolve_executable_in_path(
            "bash",
            std::ffi::OsStr::new("/usr/local/bin:/usr/bin:/bin"),
        )
        .expect("shell");
        let mut command = tokio::process::Command::new(shell);
        command
            .arg("-c")
            .arg(format!(
                "sleep 30 & echo $! > '{}'; wait",
                pid_file.display()
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().expect("spawn group");
        let process_group = i32::try_from(child.id().expect("pid")).expect("pid fits");
        let mut grouped = super::ProcessGroupChild {
            child: Some(child),
            process_group,
            complete: false,
        };
        for _ in 0..100 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let descendant: i32 = fs::read_to_string(&pid_file)
            .expect("descendant pid")
            .trim()
            .parse()
            .expect("numeric pid");
        grouped.kill_and_reap().await;
        for _ in 0..100 {
            let alive = unsafe { libc::kill(descendant, 0) == 0 };
            if !alive {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("descendant process survived process-group cancellation");
    }
}
