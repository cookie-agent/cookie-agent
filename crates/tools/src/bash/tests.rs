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
    BashArgs, BashExecutor, BashTool, read_output, resolve_executable, resolve_executable_in_path,
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
async fn output_reader_preserves_utf8_across_pipe_reads_and_flushes() {
    let root = tempfile::tempdir().unwrap();
    let call_id = ToolCallId::new_v7();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let hub = OutputHub::new(call_id, 64 * 1024);
    let progress = ProgressSink::for_test(
        sender,
        hub.clone(),
        root.path().join("artifacts"),
        cookie_agent_protocol::ToolOutputDeclaration::Named {
            streams: vec!["stdout".into(), "stderr".into()],
        },
    )
    .await
    .unwrap();
    let (mut writer, reader) = tokio::io::duplex(64);
    let merged = std::sync::Arc::new(tokio::sync::Mutex::new(super::MergedOutput::default()));
    let reading = tokio::spawn(read_output(
        reader,
        OutputStream::Stdout,
        progress,
        call_id,
        merged.clone(),
    ));
    writer.write_all(&[0xe2]).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    assert!(receiver.try_recv().is_err());
    writer.write_all(&[0x82, 0xac, b'\n']).await.unwrap();
    writer.shutdown().await.unwrap();
    reading.await.unwrap().unwrap();
    assert_eq!(merged.lock().await.text, "€\n");
    assert_eq!(
        receiver.recv().await.unwrap().display.as_deref(),
        Some("\u{20ac}\n")
    );
    let (snapshot, _) = hub.subscribe(OutputStream::Stdout, 1);
    assert_eq!(snapshot.end_offset, 4);
    assert_eq!(snapshot.chunks[0].data, "4oKsCg==");
}

#[tokio::test]
async fn output_reader_streams_bounded_display_without_buffering_full_output() {
    let root = tempfile::tempdir().unwrap();
    let call_id = ToolCallId::new_v7();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(2_048);
    let progress = ProgressSink::for_test(
        progress_tx,
        OutputHub::new(call_id, 64 * 1024),
        root.path().join("artifacts"),
        cookie_agent_protocol::ToolOutputDeclaration::Named {
            streams: vec!["stdout".into(), "stderr".into()],
        },
    )
    .await
    .unwrap();
    let (mut writer, reader) = tokio::io::duplex(2 * 1024 * 1024);
    let merged = Default::default();
    let read = tokio::spawn(read_output(
        reader,
        OutputStream::Stdout,
        progress,
        call_id,
        std::sync::Arc::clone(&merged),
    ));
    writer.write_all(b"first\n").await.expect("first output");
    let first = tokio::time::timeout(std::time::Duration::from_millis(250), progress_rx.recv())
        .await
        .expect("first chunk timeout")
        .expect("first chunk");
    assert_eq!(first.display.as_deref(), Some("first\n"));
    assert!(!read.is_finished());

    let overflow = vec![b'x'; cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES * 2 + 1];
    writer.write_all(&overflow).await.expect("overflow output");
    writer.shutdown().await.expect("output eof");
    drop(writer);

    let mut chunks = vec![first];
    while let Some(progress) = progress_rx.recv().await {
        chunks.push(progress);
    }
    read.await.expect("reader task").expect("read complete");
    assert!(merged.lock().await.text.len() <= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES);
    assert!(merged.lock().await.truncated);
    assert!(chunks.iter().all(|progress| {
        progress
            .display
            .as_ref()
            .is_none_or(|chunk| chunk.len() <= SafeDisplayText::MAX_BYTES)
    }));
    assert!(
        chunks
            .iter()
            .filter_map(|progress| progress.display.as_ref())
            .map(String::len)
            .sum::<usize>()
            <= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES
    );
}

#[tokio::test]
async fn real_bash_completion_keeps_read_order_and_nonzero_exits_are_data() {
    use cookie_agent_engine::PreparedExecutor;
    for status in [0, 1, 7] {
        let root = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let call_id = ToolCallId::new_v7();
        let executable = resolve_executable("bash").unwrap();
        let executor = BashExecutor {
            tool_call_id: call_id,
            args: BashArgs {
                command: format!(
                    "for i in 1 2 3; do printf 'stdout %s\\n' \"$i\"; sleep 0.02; printf 'stderr %s é\\n' \"$i\" >&2; sleep 0.02; done; exit {status}"
                ),
                timeout: 2_000,
                interactive: false,
            },
            cwd: crate::fs_cap::prepare_existing(Path::new("/"), root.path()).unwrap(),
            executable: crate::fs_cap::prepare_existing(Path::new("/"), &executable).unwrap(),
        };
        let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
        let mut context = cookie_agent_engine::ToolExecutionContext::for_test(
            artifacts.path().join("artifacts"),
            crate::test_turn_context(),
        )
        .unwrap();
        context.progress = ProgressSink::for_test(
            sender,
            OutputHub::new(call_id, 64 * 1024),
            artifacts.path().join("streams"),
            cookie_agent_protocol::ToolOutputDeclaration::Named {
                streams: vec!["stdout".into(), "stderr".into()],
            },
        )
        .await
        .unwrap();
        let completion = Box::new(executor).execute(context).await.unwrap();
        assert!(!completion.failed, "exit {status} is command data");
        assert_eq!(completion.result.metadata["status"], status);
        assert_eq!(completion.result.metadata["success"], status == 0);
        let mut previews = String::new();
        while let Some(progress) = receiver.recv().await {
            if let Some(display) = progress.display {
                previews.push_str(&display);
            }
        }
        // Writes arrive less than 50 ms apart, so flush-order capture would group them.
        let mut expected = String::new();
        for index in 1..=3 {
            let stdout = format!("stdout {index}\n");
            let stderr = format!("stderr {index} é\n");
            assert!(previews.contains(&stdout));
            assert!(previews.contains(&stderr));
            expected.push_str(&stdout);
            expected.push_str(&stderr);
        }
        if status != 0 {
            expected.push_str(&format!("\nExit status: {status}"));
        }
        assert_eq!(
            completion.result.display.as_deref(),
            Some(expected.as_str())
        );
    }
}

#[test]
fn merged_display_bounds_unicode_and_keeps_exit_status() {
    let mut merged = super::MergedOutput::default();
    merged.append(&"a".repeat(cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES - 1));
    merged.append("é");
    merged.append("z");
    assert!(
        !merged.text.ends_with('z'),
        "capture remains a prefix after overflow"
    );
    let display = super::completed_display(merged, Some(1));
    assert!(display.len() <= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES);
    assert!(display.ends_with("[display truncated]\nExit status: 1"));
}

#[tokio::test]
async fn signalled_bash_is_still_a_tool_failure() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let prepared = prepare(root.path(), "kill -TERM $$").await;
    let context = cookie_agent_engine::ToolExecutionContext::for_test(
        artifacts.path().join("artifacts"),
        crate::test_turn_context(),
    )
    .unwrap();
    let error = prepared.execute_for_test(context).await.unwrap_err();
    assert!(
        error.to_string().contains("terminated by a signal"),
        "{error}"
    );
}

#[tokio::test]
async fn real_bash_timeout_drains_progress_before_terminal_completion() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let root = tempfile::tempdir().expect("root");
        let artifacts = tempfile::tempdir().expect("artifact root");
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
        let progress = ProgressSink::for_test(
            progress_tx,
            OutputHub::new(call_id, 64 * 1024),
            artifacts.path().join("artifacts"),
            cookie_agent_protocol::ToolOutputDeclaration::Named {
                streams: vec!["stdout".into(), "stderr".into()],
            },
        )
        .await
        .unwrap();
        let execute =
            executor.execute_process(progress, tokio_util::sync::CancellationToken::new(), None);
        tokio::pin!(execute);
        let mut event_order = Vec::new();
        let mut progress_open = true;
        let error = loop {
            tokio::select! {
                progress = progress_rx.recv(), if progress_open => {
                    if let Some(progress) = progress {
                        if let Some(chunk) = progress.display {
                            event_order.push(("progress", chunk));
                        }
                    } else {
                        progress_open = false;
                    }
                }
                result = &mut execute => {
                    while let Ok(progress) = progress_rx.try_recv() {
                        if let Some(chunk) = progress.display {
                            event_order.push(("progress", chunk));
                        }
                    }
                    event_order.push(("terminal", String::new()));
                    break result.expect_err("bash must time out");
                }
            }
        };

        assert!(error.to_string().contains("bash timed out"), "{error}");
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
    let shell =
        resolve_executable_in_path("bash", std::ffi::OsStr::new("/usr/local/bin:/usr/bin:/bin"))
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
