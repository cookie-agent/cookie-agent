use std::time::Duration;

use cookie_agent_protocol::{RunId, SessionId, ToolCallId, in_process_pair};
use cookie_agent_runtime_api::{
    protocol::{
        AgentMdLimits, ContextParams, ContextResult, Empty, ExecuteOutcome, ExecuteParams,
        ExecuteResult, InitializeParams, InitializeResult, Outcome, PrepareOutcome, PrepareParams,
        PrepareResult, PreparedRef, RUNTIME_PROTOCOL_VERSION, RuntimeSettings, ToolOutputLimits,
        ToolProgressParams, WireToolCall, WireTurnContext, error_code, method,
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

fn initialize_params(workspace: Option<&std::path::Path>) -> InitializeParams {
    InitializeParams {
        version: RUNTIME_PROTOCOL_VERSION,
        runtime_id: "labpc".into(),
        workspace: workspace.map(|path| path.to_string_lossy().into_owned()),
        settings: settings(),
    }
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
                        .fail(RpcError::remote(error_code::METHOD_NOT_FOUND, method))
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

fn remote_code(error: RpcError) -> (i32, String) {
    match error {
        RpcError::Remote { code, message, .. } => (code, message),
        other => panic!("remote error expected, got {other:?}"),
    }
}

fn turn() -> WireTurnContext {
    WireTurnContext::from(crate::test_turn_context().as_ref())
}

#[tokio::test]
async fn a_version_mismatch_fails_initialize_with_both_versions() {
    let workspace = tempfile::tempdir().unwrap();
    let harness = start(ServeOptions::default());
    let mut params = initialize_params(Some(workspace.path()));
    params.version = RUNTIME_PROTOCOL_VERSION + 1;
    let error = harness
        .daemon
        .request::<_, InitializeResult>(method::INITIALIZE, &params)
        .await
        .expect_err("version mismatch");
    let (code, message) = remote_code(error);
    assert_eq!(code, error_code::VERSION_MISMATCH);
    assert!(
        message.contains(&format!("version {}", RUNTIME_PROTOCOL_VERSION + 1))
            && message.contains(&format!("version {RUNTIME_PROTOCOL_VERSION}")),
        "{message}"
    );
}

#[tokio::test]
async fn requests_before_initialize_and_unsupported_settings_are_refused() {
    let workspace = tempfile::tempdir().unwrap();
    let harness = start(ServeOptions::default());
    let error = harness
        .daemon
        .request::<_, ContextResult>(method::CONTEXT, &ContextParams::default())
        .await
        .expect_err("not initialized");
    assert_eq!(remote_code(error).0, error_code::NOT_INITIALIZED);

    let mut params = initialize_params(Some(workspace.path()));
    params.settings.agent_md.max_bytes = cookie_agent_runtime_api::context::AGENT_MD_MAX_BYTES + 1;
    let error = harness
        .daemon
        .request::<_, InitializeResult>(method::INITIALIZE, &params)
        .await
        .expect_err("agent_md limit above the bound");
    assert_eq!(remote_code(error).0, error_code::INITIALIZE_REJECTED);

    let error = harness
        .daemon
        .request::<_, InitializeResult>(method::INITIALIZE, &initialize_params(None))
        .await
        .expect_err("no workspace");
    assert_eq!(remote_code(error).0, error_code::INITIALIZE_REJECTED);

    // Unknown params fields are invalid params.
    let error = harness
        .daemon
        .request::<_, InitializeResult>(
            method::INITIALIZE,
            &json!({
                "version": RUNTIME_PROTOCOL_VERSION,
                "runtime_id": "labpc",
                "settings": settings(),
                "plugins": [],
            }),
        )
        .await
        .expect_err("unknown field");
    assert_eq!(remote_code(error).0, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn initialize_reports_identity_tools_and_project_skills_then_shuts_down() {
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
    let harness = start(ServeOptions {
        workspace: Some(root.clone()),
        ..ServeOptions::default()
    });
    let result: InitializeResult = harness
        .daemon
        .request(method::INITIALIZE, &initialize_params(None))
        .await
        .unwrap();
    assert_eq!(result.version, RUNTIME_PROTOCOL_VERSION);
    assert_eq!(result.kind, cookie_agent_protocol::RuntimeKind::Coding);
    assert_eq!(result.attachment_name, "labpc_my_repo");
    assert_eq!(
        result.workspace.as_deref(),
        Some(root.canonicalize().unwrap().to_str().unwrap())
    );
    assert_eq!(result.platform, std::env::consts::OS);
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

    // A second initialize is refused.
    let error = harness
        .daemon
        .request::<_, InitializeResult>(method::INITIALIZE, &initialize_params(None))
        .await
        .expect_err("already initialized");
    assert_eq!(remote_code(error).0, error_code::INITIALIZE_REJECTED);
    let error = harness
        .daemon
        .request::<_, Empty>("tool/list", &Empty {})
        .await
        .expect_err("unknown method");
    assert_eq!(remote_code(error).0, error_code::METHOD_NOT_FOUND);

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
    let _: InitializeResult = harness
        .daemon
        .request(
            method::INITIALIZE,
            &initialize_params(Some(workspace.path())),
        )
        .await
        .unwrap();
    let context: ContextResult = harness
        .daemon
        .request(
            method::CONTEXT,
            &ContextParams {
                preset: Some("python".into()),
            },
        )
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
        .request(method::CONTEXT, &ContextParams::default())
        .await
        .unwrap();
    assert!(context.agent_md.is_empty());
    assert_eq!(context.skipped.len(), 1);
    assert_eq!(context.skipped[0].byte_length, 5000);

    let error = harness
        .daemon
        .request::<_, ContextResult>(
            method::CONTEXT,
            &ContextParams {
                preset: Some("../escape".into()),
            },
        )
        .await
        .expect_err("invalid preset");
    assert_eq!(remote_code(error).0, error_code::INVALID_PARAMS);

    #[cfg(unix)]
    {
        std::fs::remove_file(workspace.path().join("AGENTS.md")).unwrap();
        std::fs::create_dir(workspace.path().join("AGENTS.md")).unwrap();
        let error = harness
            .daemon
            .request::<_, ContextResult>(method::CONTEXT, &ContextParams::default())
            .await
            .expect_err("a directory is not readable as AGENTS.md");
        let RpcError::Remote { code, data, .. } = error else {
            panic!("remote error expected");
        };
        assert_eq!(code, error_code::CONTEXT_FAILED);
        assert!(
            data.unwrap()["path"]
                .as_str()
                .unwrap()
                .ends_with("AGENTS.md")
        );
    }
}

#[tokio::test]
async fn calls_are_prepared_executed_and_released_by_prepared_id() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "hello\n").unwrap();
    let mut harness = start(ServeOptions::default());
    let _: InitializeResult = harness
        .daemon
        .request(
            method::INITIALIZE,
            &initialize_params(Some(workspace.path())),
        )
        .await
        .unwrap();
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let prepared: PrepareResult = harness
        .daemon
        .request(
            method::TOOL_PREPARE,
            &PrepareParams {
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
        cookie_agent_protocol::PermissionAction::Read
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
        prepared_id,
        session,
        run,
        turn: turn(),
        stdin: false,
        background: false,
    };
    let result: ExecuteResult = harness
        .daemon
        .request(method::TOOL_EXECUTE, &execute(read.prepared_id))
        .await
        .unwrap();
    let ExecuteOutcome::Completed { output, failed, .. } = result.outcome else {
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
    assert_eq!(remote_code(error).0, error_code::UNKNOWN_HANDLE);

    let result: ExecuteResult = harness
        .daemon
        .request(method::TOOL_EXECUTE, &execute(bash.prepared_id))
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        ExecuteOutcome::Completed { failed: false, .. }
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
    assert_eq!(remote_code(error).0, error_code::UNKNOWN_HANDLE);

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
