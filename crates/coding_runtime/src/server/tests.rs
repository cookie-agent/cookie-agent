use std::time::Duration;

use cookie_agent_protocol::{
    DiffAsOf, DisplayBlock, Open, RUNTIME_PROTOCOL_VERSION, RunId, SessionId, ToolCallId, errors,
    in_process_pair,
};
use cookie_agent_runtime_api::{
    protocol::{
        AgentMdLimits, CapabilityOpenParams, CapabilityOpenResult, ContextParams, ContextResult,
        Empty, ExecuteOutcome, ExecuteParams, ExecuteResult, InitializeParams, InitializeResult,
        Outcome, PrepareOutcome, PrepareParams, PrepareResult, PreparedRef, RuntimeSettings,
        ToolOutputLimits, ToolProgressParams, ToolResult, ToolTurn, WireToolCall, method,
    },
    rpc::{Incoming, Peer, RpcError},
};
use serde_json::json;

use super::{ServeOptions, serve};

const WAIT: Duration = Duration::from_secs(10);

fn settings() -> RuntimeSettings {
    RuntimeSettings {
        tool_output: ToolOutputLimits {
            max_lines: 100,
            max_bytes: 10_000,
            head_lines: 50,
            tail_lines: 50,
        },
        agent_md: AgentMdLimits { max_bytes: 4096 },
        mcp_servers: Vec::new(),
    }
}

fn initialize_params() -> InitializeParams {
    InitializeParams {
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        engine_version: "test".into(),
        features: Vec::new(),
        runtime_id: "labpc".into(),
        settings: settings(),
    }
}

fn open_params(workspace: Option<&std::path::Path>) -> CapabilityOpenParams {
    CapabilityOpenParams {
        capability: cookie_agent_protocol::CapabilityName::coding(),
        workspace: workspace.map(|path| path.to_string_lossy().into_owned()),
    }
}

/// Initializes the runtime and opens `coding` for `workspace`.
async fn open(harness: &Harness, workspace: &std::path::Path) -> CapabilityOpenResult {
    let _: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize_params())
        .await
        .unwrap();
    harness
        .daemon
        .request(method::CAPABILITY_OPEN, &open_params(Some(workspace)))
        .await
        .unwrap()
}

/// A served runtime and the daemon's peer. Progress requests are answered
/// and recorded.
struct Harness {
    daemon: Peer,
    server: tokio::task::JoinHandle<()>,
    progress: tokio::sync::mpsc::UnboundedReceiver<ToolProgressParams>,
}

fn start(options: ServeOptions) -> Harness {
    let (daemon_end, runtime_end) = in_process_pair(16);
    let server = tokio::spawn(serve(Box::new(runtime_end), options));
    let (daemon, mut incoming) = Peer::start(Box::new(daemon_end));
    let (progress_sender, progress) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Request(request) = message {
                if request.method == method::TOOL_PROGRESS {
                    let params: ToolProgressParams = request.params().unwrap();
                    let _ = progress_sender.send(params);
                    request.respond(&Outcome::default()).await.unwrap();
                } else {
                    let method = request.method.clone();
                    request
                        .fail(
                            errors::UNKNOWN_METHOD
                                .error()
                                .field("method", method)
                                .into(),
                        )
                        .await
                        .unwrap();
                }
            }
        }
    });
    Harness {
        daemon,
        server,
        progress,
    }
}

/// The registry code of a remote error and its text, detail included.
fn remote_code(error: RpcError) -> (String, String) {
    match error.data_code() {
        Some(code) if matches!(error, RpcError::Remote { .. }) => {
            (code.to_owned(), error.to_string())
        }
        _ => panic!("remote error with a registry code expected, got {error:?}"),
    }
}

fn turn() -> ToolTurn {
    ToolTurn::from(crate::test_turn_context().as_ref())
}

#[tokio::test]
async fn a_daemon_of_another_version_fails_initialize_with_both_versions() {
    let next_minor = cookie_agent_protocol::SemVer::new(
        RUNTIME_PROTOCOL_VERSION.major(),
        RUNTIME_PROTOCOL_VERSION.minor() + 1,
        0,
    );
    let next_major = cookie_agent_protocol::SemVer::new(RUNTIME_PROTOCOL_VERSION.major() + 1, 0, 0);
    for (version, named) in [
        (json!(next_minor), next_minor.to_string()),
        (json!(next_major), next_major.to_string()),
        // A build from before semver versions.
        (json!(6), "6".to_owned()),
    ] {
        let harness = start(ServeOptions::default());
        // Read before the rest, whose shape the other version need not share.
        let error = harness
            .daemon
            .request::<_, InitializeResult>(
                method::INITIALIZE,
                &json!({"protocol_version": version, "settings": "elsewhere"}),
            )
            .await
            .expect_err("another version");
        let (code, message) = remote_code(error);
        assert_eq!(code, "unsupported_protocol");
        assert!(
            message.contains(&format!("{RUNTIME_PROTOCOL_VERSION} here and {named}")),
            "{message}"
        );
    }
    // Another patch of the same minor connects.
    let harness = start(ServeOptions::default());
    let mut params = initialize_params();
    params.protocol_version = cookie_agent_protocol::SemVer::new(
        RUNTIME_PROTOCOL_VERSION.major(),
        RUNTIME_PROTOCOL_VERSION.minor(),
        RUNTIME_PROTOCOL_VERSION.patch() + 1,
    );
    let initialized: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &params)
        .await
        .unwrap();
    assert_eq!(initialized.protocol_version, RUNTIME_PROTOCOL_VERSION);
}

#[tokio::test]
async fn requests_before_initialize_and_unsupported_settings_are_refused() {
    let harness = start(ServeOptions::default());
    let error = harness
        .daemon
        .request::<_, ContextResult>(method::CONTEXT, &ContextParams::default())
        .await
        .expect_err("not initialized");
    assert_eq!(remote_code(error).0, "not_initialized");

    let mut params = initialize_params();
    params.settings.agent_md.max_bytes = cookie_agent_runtime_api::context::AGENT_MD_MAX_BYTES + 1;
    let error = harness
        .daemon
        .request::<_, InitializeResult>(method::INITIALIZE, &params)
        .await
        .expect_err("agent_md limit above the bound");
    assert_eq!(remote_code(error).0, "initialize_rejected");

    let _: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize_params())
        .await
        .unwrap();
    let error = harness
        .daemon
        .request::<_, CapabilityOpenResult>(method::CAPABILITY_OPEN, &open_params(None))
        .await
        .expect_err("no workspace");
    assert_eq!(remote_code(error).0, "capability_open_failed");
    let error = harness
        .daemon
        .request::<_, CapabilityOpenResult>(
            method::CAPABILITY_OPEN,
            &CapabilityOpenParams {
                capability: cookie_agent_protocol::CapabilityName::new("computer_use").unwrap(),
                workspace: None,
            },
        )
        .await
        .expect_err("no such capability");
    assert_eq!(remote_code(error).0, "capability_open_failed");
    let error = harness
        .daemon
        .request::<_, ContextResult>(method::CONTEXT, &ContextParams::default())
        .await
        .expect_err("no such instance");
    assert_eq!(remote_code(error).0, "unknown_handle");

    // Params a newer daemon sends are information: unknown members, at any
    // depth, and unknown features are ignored.
    let harness = start(ServeOptions::default());
    let mut settings = serde_json::to_value(settings()).unwrap();
    settings["plugins"] = json!([]);
    settings["tool_output"]["max_columns"] = json!(120);
    let initialized: InitializeResult = harness
        .daemon
        .request(
            method::INITIALIZE,
            &json!({
                "protocol_version": RUNTIME_PROTOCOL_VERSION,
                "engine_version": "test",
                "features": ["time_travel"],
                "runtime_id": "labpc",
                "settings": settings,
                "trace": {"id": 7},
            }),
        )
        .await
        .expect("unknown members are ignored");
    assert_eq!(initialized.protocol_version, RUNTIME_PROTOCOL_VERSION);
    let error = harness
        .daemon
        .request::<_, InitializeResult>(
            method::INITIALIZE,
            &json!({"protocol_version": RUNTIME_PROTOCOL_VERSION, "runtime_id": 7}),
        )
        .await
        .expect_err("a malformed request");
    assert_eq!(remote_code(error).0, "invalid_params");
}

#[tokio::test]
async fn opening_reports_identity_tools_and_project_skills_then_shuts_down() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().join("My Repo");
    let skill = root.join(".agents/skills/release-notes");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: release-notes\ndescription: Write release notes\n---\nCollect the changes.\n",
    )
    .unwrap();
    std::fs::write(skill.join("template.md"), "# Notes\n").unwrap();
    let other = workspace.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let harness = start(ServeOptions {
        roots: vec![workspace.path().to_owned()],
        ..ServeOptions::default()
    });
    let initialized: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize_params())
        .await
        .unwrap();
    assert_eq!(initialized.protocol_version, RUNTIME_PROTOCOL_VERSION);
    assert_eq!(initialized.platform.as_deref(), Some(std::env::consts::OS));
    let [capability] = &initialized.capabilities[..] else {
        panic!("one capability: {:?}", initialized.capabilities);
    };
    assert_eq!(capability.name.as_str(), "coding");
    assert_eq!(capability.kind.as_str(), "coding");
    assert!(capability.takes_workspace);
    let result: CapabilityOpenResult = harness
        .daemon
        .request(method::CAPABILITY_OPEN, &open_params(Some(&root)))
        .await
        .unwrap();
    assert_eq!(result.attachment_name, "labpc_my_repo");
    assert_eq!(
        result.workspace.as_deref(),
        Some(root.canonicalize().unwrap().to_str().unwrap())
    );
    let names = result
        .tools
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "write", "edit", "bash", "webfetch"]);
    assert_eq!(result.skills.skills.len(), 1);
    let skill = &result.skills.skills[0];
    assert!(skill.document.contains("Collect the changes."));
    assert!(skill.supporting_files[0].ends_with("template.md"));
    let parsed = cookie_agent_config::parse_skill_document(
        &skill.document,
        skill.source,
        std::path::Path::new(&skill.path),
        std::path::Path::new(&skill.base_dir),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(parsed.frontmatter.name, "release-notes");

    // One connection serves every workspace: the same workspace reopens its
    // instance, another opens its own, and one outside the roots is refused.
    let again: CapabilityOpenResult = harness
        .daemon
        .request(method::CAPABILITY_OPEN, &open_params(Some(&root)))
        .await
        .unwrap();
    assert_eq!(again.instance, result.instance);
    let second: CapabilityOpenResult = harness
        .daemon
        .request(method::CAPABILITY_OPEN, &open_params(Some(&other)))
        .await
        .unwrap();
    assert_ne!(second.instance, result.instance);
    assert_eq!(second.attachment_name, "labpc_other");
    let error = harness
        .daemon
        .request::<_, CapabilityOpenResult>(
            method::CAPABILITY_OPEN,
            &open_params(Some(outside.path())),
        )
        .await
        .expect_err("outside the roots");
    assert!(remote_code(error).1.contains("outside"));

    // A second initialize is refused.
    let error = harness
        .daemon
        .request::<_, InitializeResult>(method::INITIALIZE, &initialize_params())
        .await
        .expect_err("already initialized");
    assert_eq!(remote_code(error).0, "already_initialized");
    let error = harness
        .daemon
        .request::<_, Empty>("tool/list", &Empty {})
        .await
        .expect_err("unknown method");
    assert_eq!(remote_code(error).0, "method_not_found");

    let _: Empty = harness
        .daemon
        .request(method::SHUTDOWN, &Empty {})
        .await
        .unwrap();
    tokio::time::timeout(WAIT, harness.server)
        .await
        .expect("server stops")
        .unwrap();
}

#[tokio::test]
async fn context_applies_the_agent_md_lookup_and_names_unreadable_files() {
    let workspace = tempfile::tempdir().unwrap();
    let agents = workspace.path().join(".cookie-agent/agents/python");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("AGENTS.md"), "python preset").unwrap();
    std::fs::write(workspace.path().join("AGENTS.md"), "workspace").unwrap();
    let harness = start(ServeOptions::default());
    let instance = open(&harness, workspace.path()).await.instance;
    let context_params = |preset: Option<&str>| ContextParams {
        instance: instance.clone(),
        preset: preset.map(str::to_owned),
    };
    let context: ContextResult = harness
        .daemon
        .request(method::CONTEXT, &context_params(Some("python")))
        .await
        .unwrap();
    let sources = context
        .agent_md
        .iter()
        .map(|entry| (entry.source.as_str(), entry.content.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        sources,
        [
            (".cookie-agent/agents/python/AGENTS.md", "python preset"),
            ("AGENTS.md", "workspace"),
        ]
    );
    // Files over the initialize limit are skipped and reported.
    std::fs::write(workspace.path().join("AGENTS.md"), "x".repeat(5000)).unwrap();
    let context: ContextResult = harness
        .daemon
        .request(method::CONTEXT, &context_params(None))
        .await
        .unwrap();
    assert!(context.agent_md.is_empty());
    assert_eq!(context.skipped.len(), 1);
    assert_eq!(context.skipped[0].byte_length, 5000);

    let error = harness
        .daemon
        .request::<_, ContextResult>(method::CONTEXT, &context_params(Some("../escape")))
        .await
        .expect_err("invalid preset");
    assert_eq!(remote_code(error).0, "invalid_params");

    #[cfg(unix)]
    {
        std::fs::remove_file(workspace.path().join("AGENTS.md")).unwrap();
        std::fs::create_dir(workspace.path().join("AGENTS.md")).unwrap();
        let error = harness
            .daemon
            .request::<_, ContextResult>(method::CONTEXT, &context_params(None))
            .await
            .expect_err("a directory is not readable as AGENTS.md");
        assert_eq!(error.data_code(), Some("context_failed"));
        assert!(error.data_member("path").unwrap().ends_with("AGENTS.md"));
    }
}

#[tokio::test]
async fn an_edit_carries_its_preview_and_its_cut_diff_names_media_it_sends() {
    let workspace = tempfile::tempdir().unwrap();
    let text = (1..=5_000)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(workspace.path().join("big.txt"), text).unwrap();
    let harness = start(ServeOptions::default());
    let instance = Some(open(&harness, workspace.path()).await.instance);
    let (session, run) = (SessionId::new_v7(), RunId::new_v7());
    let prepared: PrepareResult = harness
        .daemon
        .request(
            method::TOOL_PREPARE,
            &PrepareParams {
                instance,
                session,
                run,
                turn: turn(),
                calls: vec![WireToolCall {
                    id: ToolCallId::new_v7(),
                    name: "edit".into(),
                    arguments: json!({
                        "filePath": "big.txt",
                        "oldString": "line",
                        "newString": "LINE",
                        "replaceAll": true
                    }),
                }],
            },
        )
        .await
        .unwrap();
    let [PrepareOutcome::Prepared(edit)] = &prepared.results[..] else {
        panic!("unexpected outcomes {:?}", prepared.results);
    };
    assert_eq!(edit.presentation.title.as_str(), "Edit big.txt");
    let [Open::Known(DisplayBlock::Diff(preview))] = edit.presentation.preview.as_slice() else {
        panic!("a diff preview: {:?}", edit.presentation.preview);
    };
    assert_eq!(preview.as_of, DiffAsOf::Prepare);
    let result: ExecuteResult = harness
        .daemon
        .request(
            method::TOOL_EXECUTE,
            &ExecuteParams {
                background: false,
                prepared_id: edit.prepared_id,
                session,
                run,
                turn: turn(),
            },
        )
        .await
        .unwrap();
    let ExecuteOutcome::Completed(ToolResult { display, .. }) = &result.outcome else {
        panic!("the edit completes");
    };
    let [Open::Known(DisplayBlock::Diff(applied))] = display.as_slice() else {
        panic!("a diff result: {display:?}");
    };
    assert_eq!(applied.as_of, DiffAsOf::Execute);
    let full = applied
        .full
        .as_ref()
        .expect("a cut diff names its full content");
    // Its bytes travel with the reply, so the daemon stores them.
    assert!(result.media.iter().any(|media| media.sha256 == full.sha256));
}

#[tokio::test]
async fn calls_are_prepared_executed_and_released_by_prepared_id() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "hello\n").unwrap();
    let mut harness = start(ServeOptions::default());
    let instance = Some(open(&harness, workspace.path()).await.instance);
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let prepared: PrepareResult = harness
        .daemon
        .request(
            method::TOOL_PREPARE,
            &PrepareParams {
                instance: instance.clone(),
                session,
                run,
                turn: turn(),
                calls: vec![
                    WireToolCall {
                        id: ToolCallId::new_v7(),
                        name: "read".into(),
                        arguments: json!({"filePath": "notes.txt"}),
                    },
                    WireToolCall {
                        id: ToolCallId::new_v7(),
                        name: "bash".into(),
                        arguments: json!({"command": "echo out; echo err >&2"}),
                    },
                    WireToolCall {
                        id: ToolCallId::new_v7(),
                        name: "grep".into(),
                        arguments: json!({}),
                    },
                ],
            },
        )
        .await
        .unwrap();
    let [
        PrepareOutcome::Prepared(read),
        PrepareOutcome::Prepared(bash),
        PrepareOutcome::Failed { error, .. },
    ] = &prepared.results[..]
    else {
        panic!("unexpected outcomes {:?}", prepared.results);
    };
    assert!(error.message.contains("unavailable"), "{}", error.message);
    assert_eq!(
        read.permission,
        Some(cookie_agent_protocol::PermissionAction::READ)
    );
    assert_eq!(read.permission_resource.as_deref(), Some("notes.txt"));
    assert_eq!(
        read.presentation
            .primary_argument
            .as_ref()
            .map(|argument| argument.as_str()),
        Some("notes.txt")
    );
    assert_eq!(
        bash.permission_resource.as_deref(),
        Some("echo out; echo err >&2")
    );

    let revalidated: Outcome = harness
        .daemon
        .request(
            method::TOOL_REVALIDATE,
            &PreparedRef {
                prepared_id: read.prepared_id,
            },
        )
        .await
        .unwrap();
    assert!(revalidated.error.is_none());

    let execute = |prepared_id| ExecuteParams {
        background: false,
        prepared_id,
        session,
        run,
        turn: turn(),
    };
    let result: ExecuteResult = harness
        .daemon
        .request(method::TOOL_EXECUTE, &execute(read.prepared_id))
        .await
        .unwrap();
    let ExecuteOutcome::Completed(ToolResult { output, failed, .. }) = result.outcome else {
        panic!("read completes");
    };
    assert!(!failed);
    let cookie_agent_protocol::ToolCompletionOutput::Single { text } = output else {
        panic!("read returns single output");
    };
    assert!(text.contains("hello"), "{text}");

    // Executing consumes the prepared call.
    let error = harness
        .daemon
        .request::<_, ExecuteResult>(method::TOOL_EXECUTE, &execute(read.prepared_id))
        .await
        .expect_err("consumed");
    assert_eq!(remote_code(error).0, "unknown_handle");

    let result: ExecuteResult = harness
        .daemon
        .request(method::TOOL_EXECUTE, &execute(bash.prepared_id))
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        ExecuteOutcome::Completed(ToolResult { failed: false, .. })
    ));
    let mut streamed = String::new();
    while let Ok(progress) = harness.progress.try_recv() {
        assert_eq!(progress.prepared_id, bash.prepared_id);
        for chunk in progress.progress.output {
            streamed.push_str(&chunk.text);
        }
    }
    assert!(
        streamed.contains("out") && streamed.contains("err"),
        "{streamed}"
    );

    // A released call is gone.
    let prepared: PrepareResult = harness
        .daemon
        .request(
            method::TOOL_PREPARE,
            &PrepareParams {
                instance: instance.clone(),
                session,
                run,
                turn: turn(),
                calls: vec![WireToolCall {
                    id: ToolCallId::new_v7(),
                    name: "read".into(),
                    arguments: json!({"filePath": "notes.txt"}),
                }],
            },
        )
        .await
        .unwrap();
    let PrepareOutcome::Prepared(read) = &prepared.results[0] else {
        panic!("read prepares");
    };
    harness
        .daemon
        .notify(
            method::TOOL_RELEASE,
            &PreparedRef {
                prepared_id: read.prepared_id,
            },
        )
        .await
        .unwrap();
    let error = harness
        .daemon
        .request::<_, Outcome>(
            method::TOOL_REVALIDATE,
            &PreparedRef {
                prepared_id: read.prepared_id,
            },
        )
        .await
        .expect_err("released");
    assert_eq!(remote_code(error).0, "unknown_handle");

    // Transport EOF stops the server.
    harness.daemon.close();
    tokio::time::timeout(WAIT, harness.server)
        .await
        .expect("server stops on EOF")
        .unwrap();
}

#[test]
fn a_leading_tilde_expands_to_the_home_directory() {
    use std::path::Path;

    let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
    assert_eq!(super::expand_home(Path::new("~")), home);
    assert_eq!(
        super::expand_home(Path::new("~/src/cookie-agent")),
        home.join("src/cookie-agent")
    );
    for unchanged in ["~other/src", "src/~", "/srv/~/x", "relative"] {
        assert_eq!(
            super::expand_home(Path::new(unchanged)),
            Path::new(unchanged),
            "{unchanged}"
        );
    }
}
