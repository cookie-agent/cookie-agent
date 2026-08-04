use std::{
    env,
    ffi::OsString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::PersistedToolResult as ToolResult;
use cookie_agent_protocol::{
    ApprovalResourceSource, OutputStream, PermissionAction, PreparedBindingLifetime,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

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
    args: BashArgs,
    cwd: fs_cap::PreparedExisting,
    executable: fs_cap::PreparedExisting,
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
            description: "Execute one prepared shell command.".into(),
            parameters: schema::<BashArgs>(),
        }])
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
        if args.interactive {
            return Err(ToolError::unsupported_security(
                "interactive prepared bash is not supported",
            ));
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
        let subcommands = parsed_subcommands(&args.command);
        let mut resources = Vec::with_capacity(subcommands.len());
        let mut policy_labels = Vec::with_capacity(subcommands.len());
        for (index, subcommand) in subcommands.into_iter().enumerate() {
            let mut binding = args.command.as_bytes().to_vec();
            binding.extend_from_slice(&executable_binding);
            binding.extend_from_slice(&(index as u64).to_be_bytes());
            resources.push(prepared_resource(
                PermissionAction::Bash,
                "command",
                subcommand.as_bytes(),
                &binding,
                PreparedBindingLifetime::ProcessLocal,
                ApprovalResourceSource::PrimaryOperation,
            )?);
            policy_labels.push(subcommand);
        }
        let mut context = cwd.manifest_bytes()?;
        context.extend_from_slice(&executable_binding);
        let operation = prepared_operation(
            "bash",
            &args,
            vec![(PermissionAction::Bash, "execute")],
            resources,
            &context,
        )?;
        PreparedTool::new(
            operation,
            None,
            Box::new(BashExecutor {
                args,
                cwd,
                executable,
            }),
        )
        .with_policy_labels(policy_labels)
    }
}

fn parsed_subcommands(command: &str) -> Vec<String> {
    let fallback = || vec![command.trim().to_owned()];
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return fallback();
    }
    let Some(tree) = parser.parse(command, None) else {
        return fallback();
    };
    if tree.root_node().has_error() {
        return fallback();
    }
    let mut nodes = vec![tree.root_node()];
    let mut commands = Vec::new();
    while let Some(node) = nodes.pop() {
        if node.kind() == "command"
            && let Ok(value) = node.utf8_text(command.as_bytes())
        {
            let value = value.trim();
            if !value.is_empty() {
                commands.push((node.start_byte(), value.to_owned()));
            }
        }
        let mut cursor = node.walk();
        nodes.extend(node.children(&mut cursor));
    }
    commands.sort_by_key(|(offset, _)| *offset);
    if commands.is_empty() {
        fallback()
    } else {
        commands.into_iter().map(|(_, value)| value).collect()
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
        self.cwd.revalidate()?;
        self.executable.revalidate()?;
        if context.cancellation.is_cancelled() {
            return Err(ToolError::execution("prepared bash cancelled"));
        }
        let mut command = Command::new(self.executable.proc_fd_path());
        command
            .arg("-lc")
            .arg(&self.args.command)
            .current_dir(self.cwd.proc_fd_path())
            .stdin(Stdio::null())
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
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::execution("bash stdout pipe missing"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::execution("bash stderr pipe missing"))?;
        let stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });
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
                _ = context.cancellation.cancelled() => WaitOutcome::Cancelled,
            }
        };
        let status = match outcome {
            WaitOutcome::Finished(result) => {
                grouped.complete = true;
                result.map_err(|error| ToolError::execution(error.to_string()))?
            }
            WaitOutcome::TimedOut => {
                grouped.kill_and_reap().await;
                return Err(ToolError::execution("bash timed out"));
            }
            WaitOutcome::Cancelled => {
                grouped.kill_and_reap().await;
                return Err(ToolError::execution("prepared bash cancelled"));
            }
        };
        grouped.kill_group();
        let stdout = stdout_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let stderr = stderr_task
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?
            .map_err(|error| ToolError::execution(error.to_string()))?;
        context.progress.output(OutputStream::Stdout, &stdout);
        context.progress.output(OutputStream::Stderr, &stderr);
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

    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{RunId, SessionId, ToolCallId};

    use super::{BashTool, parsed_subcommands, resolve_executable_in_path};

    #[test]
    fn parsed_subcommands_are_evaluated_once_in_source_order() {
        assert_eq!(
            parsed_subcommands("git status && printf '%s' ok; cargo check"),
            ["git status", "printf '%s' ok", "cargo check"]
        );
        assert_eq!(parsed_subcommands("echo $(pwd)"), ["echo $(pwd)", "pwd"]);
    }

    #[test]
    fn unsafe_parse_falls_back_to_whole_command() {
        assert_eq!(
            parsed_subcommands("echo 'unterminated"),
            ["echo 'unterminated"]
        );
    }

    #[tokio::test]
    async fn prepared_bash_has_one_permission_resource_per_subcommand() {
        let root = tempfile::tempdir().expect("root");
        let prepared = BashTool::new(root.path())
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.path().to_owned(),
                    workspace_root: root.path().to_owned(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"echo one; echo one"}),
                },
            )
            .await
            .expect("prepare");
        assert_eq!(prepared.policy_labels(), ["echo one", "echo one"]);
        assert_eq!(prepared.operation().resources().len(), 2);
        assert_ne!(
            prepared.operation().resources()[0].binding_digest,
            prepared.operation().resources()[1].binding_digest
        );
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
    async fn interactive_is_rejected_during_prepare() {
        let root = tempfile::tempdir().expect("root");
        let tool = BashTool::new(root.path());
        let result = tool
            .prepare(
                cookie_agent_engine::ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.path().to_owned(),
                    workspace_root: root.path().to_owned(),
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
        assert!(matches!(result, Err(ToolError::UnsupportedSecurity(_))));
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
