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
        let parsed = parsed_command_line(&args.command);
        let specifications = permission_resources(&parsed);
        let mut resources = Vec::with_capacity(specifications.len());
        let mut policy_labels = Vec::with_capacity(specifications.len());
        let mut capabilities = Vec::new();
        for (index, specification) in specifications.into_iter().enumerate() {
            let mut binding = args.command.as_bytes().to_vec();
            binding.extend_from_slice(&executable_binding);
            binding.extend_from_slice(&(index as u64).to_be_bytes());
            resources.push(prepared_resource(
                specification.action,
                if specification.action == PermissionAction::Bash {
                    "command"
                } else {
                    "path"
                },
                specification.label.as_bytes(),
                &binding,
                PreparedBindingLifetime::ProcessLocal,
                ApprovalResourceSource::PrimaryOperation,
            )?);
            policy_labels.push(specification.label);
            if !capabilities
                .iter()
                .any(|(action, _)| *action == specification.action)
            {
                capabilities.push((
                    specification.action,
                    match specification.action {
                        PermissionAction::Read => "read",
                        PermissionAction::Write => "write",
                        _ => "execute",
                    },
                ));
            }
        }
        let mut context = cwd.manifest_bytes()?;
        context.extend_from_slice(&executable_binding);
        let operation = prepared_operation("bash", &args, capabilities, resources, &context)?;
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

#[derive(Debug, Eq, PartialEq)]
struct ParsedCommandLine {
    subcommands: Vec<ParsedSubcommand>,
    simple_pipeline: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedSubcommand {
    text: String,
    name: Option<String>,
    arguments: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct PermissionResourceSpecification {
    action: PermissionAction,
    label: String,
}

/// The fixed first-token classification table for simple bash commands.
const READ_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "pwd", "echo", "find", "grep", "rg", "wc", "file", "stat", "tree",
];
const WRITE_COMMANDS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "mkdir", "touch", "chmod", "chown", "ln", "tee", "truncate",
];

fn parsed_command_line(command: &str) -> ParsedCommandLine {
    let fallback = || ParsedCommandLine {
        subcommands: vec![ParsedSubcommand {
            text: command.trim().to_owned(),
            name: None,
            arguments: Vec::new(),
        }],
        simple_pipeline: false,
    };
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
    let root = tree.root_node();
    let simple_nodes = simple_command_nodes(root, command);
    let mut nodes = vec![root];
    let mut commands = Vec::new();
    while let Some(node) = nodes.pop() {
        if node.kind() == "command"
            && let Ok(value) = node.utf8_text(command.as_bytes())
        {
            let value = value.trim();
            if !value.is_empty() {
                commands.push((node.start_byte(), parsed_subcommand(node, command, value)));
            }
        }
        let mut cursor = node.walk();
        nodes.extend(node.children(&mut cursor));
    }
    commands.sort_by_key(|(offset, _)| *offset);
    if commands.is_empty() {
        fallback()
    } else {
        let simple_pipeline = simple_nodes.is_some();
        let subcommands = if let Some(simple_nodes) = simple_nodes {
            simple_nodes
                .into_iter()
                .filter_map(|node| {
                    let value = node.utf8_text(command.as_bytes()).ok()?.trim();
                    (!value.is_empty()).then(|| parsed_subcommand(node, command, value))
                })
                .collect()
        } else {
            commands.into_iter().map(|(_, value)| value).collect()
        };
        ParsedCommandLine {
            subcommands,
            simple_pipeline,
        }
    }
}

fn simple_command_nodes<'tree>(
    root: tree_sitter::Node<'tree>,
    command: &str,
) -> Option<Vec<tree_sitter::Node<'tree>>> {
    if command.contains('\n') || root.named_child_count() != 1 {
        return None;
    }
    let statement = root.named_child(0)?;
    if command.trim() != statement.utf8_text(command.as_bytes()).ok()?.trim() {
        return None;
    }
    match statement.kind() {
        "command" if command_node_is_simple(statement) => Some(vec![statement]),
        "pipeline" => {
            let mut commands = Vec::new();
            let mut cursor = statement.walk();
            for child in statement.children(&mut cursor) {
                if child.is_named() {
                    if child.kind() != "command" || !command_node_is_simple(child) {
                        return None;
                    }
                    commands.push(child);
                } else if child.kind() != "|" {
                    return None;
                }
            }
            (!commands.is_empty()).then_some(commands)
        }
        _ => None,
    }
}

fn command_node_is_simple(command: tree_sitter::Node<'_>) -> bool {
    let mut nodes = vec![command];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "command_substitution"
                | "file_redirect"
                | "heredoc_redirect"
                | "herestring_redirect"
                | "redirected_statement"
                | "subshell"
                | "compound_statement"
        ) {
            return false;
        }
        let mut cursor = node.walk();
        nodes.extend(node.children(&mut cursor));
    }
    true
}

fn parsed_subcommand(node: tree_sitter::Node<'_>, source: &str, text: &str) -> ParsedSubcommand {
    let name = node
        .child_by_field_name("name")
        .and_then(|name| bare_shell_word(name, source));
    let mut arguments = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some("argument")
                && let Some(argument) = bare_shell_word(cursor.node(), source)
            {
                arguments.push(argument);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    ParsedSubcommand {
        text: text.to_owned(),
        name,
        arguments,
    }
}

fn bare_shell_word(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut nodes = vec![node];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "expansion" | "simple_expansion" | "command_substitution"
        ) {
            return None;
        }
        let mut cursor = node.walk();
        nodes.extend(node.children(&mut cursor));
    }
    let value = node.utf8_text(source.as_bytes()).ok()?.trim();
    let value = if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };
    Some(value.to_owned())
}

fn permission_resources(parsed: &ParsedCommandLine) -> Vec<PermissionResourceSpecification> {
    if !parsed.simple_pipeline {
        return parsed
            .subcommands
            .iter()
            .map(|subcommand| PermissionResourceSpecification {
                action: PermissionAction::Bash,
                label: subcommand.text.clone(),
            })
            .collect();
    }
    parsed
        .subcommands
        .iter()
        .flat_map(|subcommand| {
            let action = classified_action(subcommand.name.as_deref());
            if action == PermissionAction::Bash {
                return vec![PermissionResourceSpecification {
                    action,
                    label: subcommand.text.clone(),
                }];
            }
            let labels = file_arguments(subcommand);
            if labels.is_empty() {
                vec![PermissionResourceSpecification {
                    action,
                    label: subcommand.text.clone(),
                }]
            } else {
                labels
                    .into_iter()
                    .map(|label| PermissionResourceSpecification { action, label })
                    .collect()
            }
        })
        .collect()
}

fn classified_action(name: Option<&str>) -> PermissionAction {
    match name {
        Some(name) if READ_COMMANDS.contains(&name) => PermissionAction::Read,
        Some(name) if WRITE_COMMANDS.contains(&name) => PermissionAction::Write,
        _ => PermissionAction::Bash,
    }
}

fn file_arguments(command: &ParsedSubcommand) -> Vec<String> {
    let name = command.name.as_deref().unwrap_or_default();
    if name == "find" {
        return find_path_arguments(&command.arguments);
    }
    let mut positional = positional_arguments(name, &command.arguments);
    match name {
        "pwd" | "echo" => Vec::new(),
        "grep" | "rg" if !has_pattern_option(&command.arguments) => {
            positional.drain(..positional.len().min(1));
            positional
        }
        "chmod" | "chown" if !has_reference_option(&command.arguments) => {
            positional.drain(..positional.len().min(1));
            positional
        }
        _ => positional,
    }
}

fn positional_arguments(command: &str, arguments: &[String]) -> Vec<String> {
    let mut positional = Vec::new();
    let mut options = true;
    let mut skip_value = false;
    for argument in arguments {
        if skip_value {
            skip_value = false;
            continue;
        }
        if options && argument == "--" {
            options = false;
        } else if options && argument.starts_with('-') && argument != "-" {
            skip_value = option_takes_separate_value(command, argument);
        } else {
            positional.push(argument.clone());
        }
    }
    positional
}

fn option_takes_separate_value(command: &str, option: &str) -> bool {
    if option.contains('=') {
        return false;
    }
    matches!(
        (command, option),
        ("head" | "tail", "-n" | "--lines" | "-c" | "--bytes")
            | (
                "ls",
                "-I" | "--ignore" | "--hide" | "--sort" | "--time" | "--format"
            )
            | (
                "grep" | "rg",
                "-e" | "--regexp" | "-f" | "--file" | "-m" | "--max-count"
            )
            | ("stat", "-c" | "--format" | "--printf")
            | ("tree", "-L" | "-P" | "-I" | "--filelimit")
            | ("cp" | "mv" | "ln", "-t" | "--target-directory")
            | ("truncate", "-s" | "--size" | "-r" | "--reference")
            | ("touch", "-d" | "--date" | "-r" | "--reference" | "-t")
            | ("mkdir", "-m" | "--mode")
            | ("chmod", "--reference")
            | ("chown", "--reference" | "--from")
    )
}

fn has_pattern_option(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        matches!(argument.as_str(), "-e" | "--regexp" | "-f" | "--file")
            || (argument.starts_with("-e") && argument.len() > 2)
            || (argument.starts_with("-f") && argument.len() > 2)
            || argument.starts_with("--regexp=")
            || argument.starts_with("--file=")
    })
}

fn has_reference_option(arguments: &[String]) -> bool {
    arguments
        .iter()
        .any(|argument| argument == "--reference" || argument.starts_with("--reference="))
}

fn find_path_arguments(arguments: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument.as_str(), "-H" | "-L" | "-P") || argument.starts_with("-O") {
            index += 1;
            continue;
        }
        if argument == "-D" {
            index += 2;
            continue;
        }
        break;
    }
    while let Some(argument) = arguments.get(index) {
        if argument.starts_with('-') || matches!(argument.as_str(), "!" | "(" | ")" | ",") {
            break;
        }
        paths.push(argument.clone());
        index += 1;
    }
    paths
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

    use cookie_agent_engine::permissions::PermissionPipeline;
    use cookie_agent_engine::{ToolCall, ToolError, ToolPreparationContext, ToolProvider};
    use cookie_agent_protocol::{
        AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot,
        PermissionAction, PermissionEffect, PermissionRule, RunId, SessionId, Sha256Digest,
        ToolCallId, WildcardPattern,
    };

    use super::{BashTool, parsed_command_line, resolve_executable_in_path};

    async fn prepare(root: &Path, command: &str) -> cookie_agent_engine::PreparedTool {
        BashTool::new(root)
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: root.to_owned(),
                    workspace_root: root.to_owned(),
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
            tools: Vec::new(),
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

    #[test]
    fn parsed_subcommands_are_evaluated_once_in_source_order() {
        assert_eq!(
            parsed_command_line("git status && printf '%s' ok; cargo check")
                .subcommands
                .into_iter()
                .map(|command| command.text)
                .collect::<Vec<_>>(),
            ["git status", "printf '%s' ok", "cargo check"]
        );
        assert_eq!(
            parsed_command_line("echo $(pwd)")
                .subcommands
                .into_iter()
                .map(|command| command.text)
                .collect::<Vec<_>>(),
            ["echo $(pwd)", "pwd"]
        );
    }

    #[test]
    fn unsafe_parse_falls_back_to_whole_command() {
        assert_eq!(
            parsed_command_line("echo 'unterminated")
                .subcommands
                .into_iter()
                .map(|command| command.text)
                .collect::<Vec<_>>(),
            ["echo 'unterminated"]
        );
    }

    #[tokio::test]
    async fn complex_bash_has_one_permission_resource_per_parsed_subcommand() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "echo one; echo one").await;
        assert_eq!(prepared.policy_labels(), ["echo one", "echo one"]);
        assert_eq!(prepared.operation().resources().len(), 2);
        assert!(
            prepared
                .operation()
                .resources()
                .iter()
                .all(|resource| resource.capability == PermissionAction::Bash)
        );
        assert_ne!(
            prepared.operation().resources()[0].binding_digest,
            prepared.operation().resources()[1].binding_digest
        );
    }

    #[tokio::test]
    async fn simple_read_commands_use_file_labels_and_read_policy() {
        let root = tempfile::tempdir().expect("root");
        let policy = policy(vec![
            rule(PermissionAction::Read, "*", PermissionEffect::Allow),
            rule(PermissionAction::Read, ".env", PermissionEffect::Deny),
        ]);
        let env = prepare(root.path(), "cat .env").await;
        let env_decision = PermissionPipeline::default().decide_operation(
            &policy,
            env.operation(),
            env.policy_labels(),
            root.path(),
        );
        assert_eq!(env.policy_labels(), [".env"]);
        assert_eq!(
            env.operation().resources()[0].capability,
            PermissionAction::Read
        );
        assert_eq!(env_decision.effect, PermissionEffect::Deny);
        assert_eq!(
            env_decision.evaluations[0].trace.action,
            PermissionAction::Read
        );
        assert_eq!(
            env_decision.evaluations[0].trace.normalized_resource,
            ".env"
        );

        let ordinary = prepare(root.path(), "ls ordinary/").await;
        let ordinary_decision = PermissionPipeline::default().decide_operation(
            &policy,
            ordinary.operation(),
            ordinary.policy_labels(),
            root.path(),
        );
        assert_eq!(ordinary.policy_labels(), ["ordinary/"]);
        assert_eq!(ordinary_decision.effect, PermissionEffect::Allow);
    }

    #[tokio::test]
    async fn write_commands_skip_flags_and_flag_values() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "rm -rf build/").await;
        assert_eq!(prepared.policy_labels(), ["build/"]);
        assert_eq!(
            prepared.operation().resources()[0].capability,
            PermissionAction::Write
        );

        let tail = prepare(root.path(), "tail -n 10 log.txt").await;
        assert_eq!(tail.policy_labels(), ["log.txt"]);
        assert_eq!(
            tail.operation().resources()[0].capability,
            PermissionAction::Read
        );
    }

    #[tokio::test]
    async fn simple_pipeline_segments_are_rerouted_individually() {
        let root = tempfile::tempdir().expect("root");
        let read = prepare(root.path(), "ls . | tail -n 10").await;
        assert_eq!(read.policy_labels(), [".", "tail -n 10"]);
        assert!(
            read.operation()
                .resources()
                .iter()
                .all(|resource| resource.capability == PermissionAction::Read)
        );

        let mixed = prepare(root.path(), "ls | tee out.txt").await;
        assert_eq!(mixed.policy_labels(), ["ls", "out.txt"]);
        assert_eq!(
            mixed
                .operation()
                .resources()
                .iter()
                .map(|resource| resource.capability)
                .collect::<Vec<_>>(),
            [PermissionAction::Read, PermissionAction::Write]
        );
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![
                rule(PermissionAction::Read, "*", PermissionEffect::Allow),
                rule(PermissionAction::Write, "*", PermissionEffect::Ask),
            ]),
            mixed.operation(),
            mixed.policy_labels(),
            root.path(),
        );
        assert_eq!(decision.effect, PermissionEffect::Ask);
    }

    #[tokio::test]
    async fn complex_shell_constructs_keep_every_resource_on_bash() {
        let root = tempfile::tempdir().expect("root");
        for command in ["git status", "ls && rm x", "ls > out", "cat $(f)"] {
            let prepared = prepare(root.path(), command).await;
            assert!(
                prepared
                    .operation()
                    .resources()
                    .iter()
                    .all(|resource| resource.capability == PermissionAction::Bash)
            );
        }
    }

    #[tokio::test]
    async fn rerouted_command_without_file_arguments_uses_subcommand_label() {
        let root = tempfile::tempdir().expect("root");
        let prepared = prepare(root.path(), "pwd").await;
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![rule(
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            )]),
            prepared.operation(),
            prepared.policy_labels(),
            root.path(),
        );
        assert_eq!(prepared.policy_labels(), ["pwd"]);
        assert_eq!(decision.effect, PermissionEffect::Allow);
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
