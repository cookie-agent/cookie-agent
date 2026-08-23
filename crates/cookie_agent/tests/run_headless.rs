#![cfg(unix)]

use std::{
    collections::VecDeque,
    fs,
    future::pending,
    io::{Cursor, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use cookie_agent::run::{AllowedTool, OutputMode, PermissionModeArg, RunArgs, execute_with_io};
use cookie_agent_config::load;
use cookie_agent_engine::{Engine, EngineHistoryView, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogManager, CatalogRequest, CatalogTransport, CatalogTransportFuture,
        CatalogTransportResponse, MODELS_DEV_BOOTSTRAP,
    },
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    ClientRunId, EventPayload, EventSubscriptionMessage, PermissionAction, PermissionEffect,
    RunSelection, RunStartParams, SessionId, StoredEvent,
};
use cookie_agent_tools::{BuiltinTools, delegate::DelegateToolProvider, skill::SkillTool};
use tempfile::TempDir;

const PLUGIN_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../engine/tests/fixtures/fake_plugin.py"
);

enum MockResponse {
    Sse(String),
    GatedSse {
        body: String,
        start: std_mpsc::Receiver<()>,
        deadline: Instant,
    },
    Status(u16),
    Delay(Duration),
}

struct MockModelServer {
    address: std::net::SocketAddr,
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl MockModelServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock model server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking mock listener");
        let address = listener.local_addr().expect("mock address");
        let responses = Arc::new(Mutex::new(VecDeque::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let task_responses = Arc::clone(&responses);
        let task_requests = Arc::clone(&requests);
        let task_stop = Arc::clone(&stop);
        let task = thread::spawn(move || {
            while !task_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if task_stop.load(Ordering::Acquire) {
                            break;
                        }
                        let Some(request) = read_request(&mut stream) else {
                            continue;
                        };
                        task_requests.lock().expect("request capture").push(request);
                        let Some(response) =
                            task_responses.lock().expect("response queue").pop_front()
                        else {
                            continue;
                        };
                        match response {
                            MockResponse::Sse(body) => write_sse(&mut stream, &body),
                            MockResponse::GatedSse {
                                body,
                                start,
                                deadline,
                            } => {
                                start
                                    .recv_timeout(deadline_remaining(
                                        deadline,
                                        "mock server waiting for the output gate",
                                    ))
                                    .unwrap_or_else(|error| {
                                        panic!("output gate did not open before deadline: {error}")
                                    });
                                write_sse(&mut stream, &body);
                            }
                            MockResponse::Status(status) => write_status(&mut stream, status),
                            MockResponse::Delay(duration) => thread::sleep(duration),
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock model accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            responses,
            requests,
            stop,
            task: Some(task),
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn enqueue(&self, response: MockResponse) {
        self.responses
            .lock()
            .expect("response queue")
            .push_back(response);
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Drop for MockModelServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(task) = self.task.take() {
            task.join().expect("join mock model server");
        }
    }
}

#[derive(Clone, Copy)]
struct BundledCatalogTransport;

impl CatalogTransport for BundledCatalogTransport {
    fn fetch(&self, _request: CatalogRequest) -> CatalogTransportFuture<'_> {
        Box::pin(async {
            Ok(CatalogTransportResponse::from_bytes(
                200,
                MODELS_DEV_BOOTSTRAP.to_vec(),
            ))
        })
    }
}

struct Fixture {
    _root: TempDir,
    workspace: PathBuf,
    engine: Engine,
    server: MockModelServer,
}

struct ProcessFixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    data_dir: PathBuf,
    server: MockModelServer,
}

impl ProcessFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("process fixture root");
        make_private(root.path());
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let data_dir = root.path().join("data");
        fs::create_dir(&home).expect("process home");
        fs::create_dir(&workspace).expect("process workspace");
        make_private(&home);
        make_private(&workspace);
        let server = MockModelServer::start();
        write_workspace(&workspace, &server.endpoint());
        Self {
            _root: root,
            home,
            workspace,
            data_dir,
            server,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.home)
            .env("COOKIE_AGENT_TEST_BUNDLED_CATALOG", "1");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command()
            .args(arguments)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .output()
            .expect("run cookie binary")
    }
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        make_private(root.path());
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        make_private(&workspace);
        let server = MockModelServer::start();
        write_workspace(&workspace, &server.endpoint());

        let catalog = Arc::new(
            CatalogManager::in_directory(BundledCatalogTransport, root.path(), "catalog")
                .refresh()
                .await
                .expect("bundled catalog"),
        );
        let provider_store_path = root.path().join("providers");
        fs::create_dir(&provider_store_path).expect("provider store directory");
        make_private(&provider_store_path);
        let configuration = load(&workspace).expect("test configuration");
        let model_manager = Arc::new(
            ModelManager::new(
                configuration.runtime.providers.clone(),
                catalog,
                ProviderStore::open(&provider_store_path).expect("provider store"),
            )
            .expect("model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: root.path().join("data"),
            cwd: workspace.clone(),
            config: configuration,
            model_manager,
            tools: vec![Arc::new(BuiltinTools::new(&workspace))],
        })
        .expect("engine");
        engine
            .try_register_tool_provider(Arc::new(DelegateToolProvider::new(engine.clone())))
            .expect("delegate tools");
        engine
            .try_register_tool_provider(Arc::new(SkillTool::new(engine.clone())))
            .expect("skill tool");
        Self {
            _root: root,
            workspace,
            engine,
            server,
        }
    }

    async fn run(&self, args: RunArgs, stdin: &str) -> RunResult {
        self.run_with_interrupt(args, stdin, pending()).await
    }

    async fn run_with_interrupt<F>(&self, args: RunArgs, stdin: &str, interrupt: F) -> RunResult
    where
        F: Future<Output = ()>,
    {
        let mut input = Cursor::new(stdin.as_bytes());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = execute_with_io(
            &self.engine,
            args,
            &mut input,
            &mut stdout,
            &mut stderr,
            interrupt,
        )
        .await;
        RunResult {
            code,
            stdout: String::from_utf8(stdout).expect("UTF-8 stdout"),
            stderr: String::from_utf8(stderr).expect("UTF-8 stderr"),
        }
    }

    async fn shutdown(&self) {
        self.engine.shutdown().await;
    }
}

struct RunResult {
    code: i32,
    stdout: String,
    stderr: String,
}

#[derive(Default)]
struct SlowLineWriter {
    bytes: Vec<u8>,
    delayed: bool,
}

impl std::io::Write for SlowLineWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer == b"\n" && !self.delayed {
            self.delayed = true;
            thread::sleep(Duration::from_secs(2));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct GatedLineWriter {
    bytes: Vec<u8>,
    start: Option<std_mpsc::SyncSender<()>>,
    terminal: Option<std_mpsc::Receiver<Result<(), String>>>,
    deadline: Instant,
}

impl GatedLineWriter {
    fn new(
        start: std_mpsc::SyncSender<()>,
        terminal: std_mpsc::Receiver<Result<(), String>>,
        deadline: Instant,
    ) -> Self {
        Self {
            bytes: Vec::new(),
            start: Some(start),
            terminal: Some(terminal),
            deadline,
        }
    }
}

impl std::io::Write for GatedLineWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer == b"\n"
            && let Some(start) = self.start.take()
        {
            start
                .try_send(())
                .expect("mock server dropped the output gate before it opened");
            let terminal = self
                .terminal
                .take()
                .expect("terminal monitor missing when output gate opened");
            terminal
                .recv_timeout(deadline_remaining(
                    self.deadline,
                    "output writer waiting for the terminal event",
                ))
                .unwrap_or_else(|error| {
                    panic!("terminal event was not observed before deadline: {error}")
                })
                .unwrap_or_else(|error| panic!("terminal event monitor failed: {error}"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn terminal_signal(
    engine: &Engine,
    session_id: SessionId,
    deadline: Instant,
) -> std_mpsc::Receiver<Result<(), String>> {
    let engine = engine.clone();
    let (sender, receiver) = std_mpsc::sync_channel(1);
    tokio::spawn(async move {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        let result =
            tokio::time::timeout(remaining, wait_for_session_terminal(&engine, session_id))
                .await
                .unwrap_or_else(|_| Err("timed out waiting for a terminal session event".into()));
        let _ = sender.try_send(result);
    });
    receiver
}

fn deadline_remaining(deadline: Instant, wait: &str) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_else(|| panic!("{wait} exceeded the coordination deadline"))
}

async fn wait_for_session_terminal(engine: &Engine, session_id: SessionId) -> Result<(), String> {
    let mut cursor = None;
    loop {
        let (replay, mut receiver) = engine
            .subscribe(session_id, cursor)
            .await
            .map_err(|error| error.to_string())?;
        for event in replay.events {
            if observe_terminal(&event, &mut cursor) {
                return Ok(());
            }
        }
        while let Some(EventSubscriptionMessage::Event { event }) = receiver.recv().await {
            if observe_terminal(&event, &mut cursor) {
                return Ok(());
            }
        }
    }
}

fn observe_terminal(event: &StoredEvent, cursor: &mut Option<u64>) -> bool {
    *cursor = Some(event.seq);
    matches!(
        event.payload,
        EventPayload::RunCompleted { .. }
            | EventPayload::RunFailed { .. }
            | EventPayload::RunCancelled { .. }
            | EventPayload::RunInterrupted { .. }
    )
}

#[tokio::test]
async fn headless_outputs_prompt_sources_selection_and_verbose_tool_output() {
    let fixture = Fixture::new().await;

    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("terminal only")));
    let mut args = run_args("ignored");
    args.positional_prompt = None;
    args.prompt = Some("-".into());
    let text = fixture.run(args, "stdin prompt").await;
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert_eq!(text.stdout, "terminal only\n");
    assert!(text.stderr.is_empty());
    let first_request = fixture
        .server
        .requests()
        .into_iter()
        .next()
        .expect("request");
    assert!(
        first_request.contains("\"model\":\"test\""),
        "first live fallback was not selected: {first_request}"
    );

    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("alternate model")));
    let mut args = run_args("selection prompt");
    args.output = Some(OutputMode::Json);
    args.model = Some("custom.local/alternate".parse().expect("model key"));
    args.variant = Some("base".into());
    let json = fixture.run(args, "").await;
    assert_eq!(json.code, 0, "{}", json.stderr);
    assert!(json.stderr.is_empty());
    let records = parse_json_lines(&json.stdout);
    assert_jsonl_structure(&records);
    let run_started = records
        .iter()
        .find_map(|record| {
            (record["type"] == "event" && record["event"]["payload"]["type"] == "run_started")
                .then_some(&record["event"]["payload"])
        })
        .expect("run_started record");
    assert_eq!(
        run_started["selection"]["model"]["model"],
        "custom.local/alternate"
    );
    let summary = records.last().expect("summary");
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["exit_code"], 0);
    assert_eq!(summary["usage"]["input_tokens"], 10);
    assert_eq!(summary["usage"]["output_tokens"], 2);
    let resumed_session: SessionId =
        serde_json::from_value(summary["session_id"].clone()).expect("session ID");
    assert!(
        fixture
            .server
            .requests()
            .iter()
            .any(|request| request.contains("selection prompt"))
    );

    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("resumed session")));
    let mut args = run_args("resume prompt");
    args.resume_session = Some(resumed_session);
    args.output = Some(OutputMode::None);
    let resumed = fixture.run(args, "").await;
    assert_eq!(resumed.code, 0, "{}", resumed.stderr);
    assert!(resumed.stdout.is_empty());

    let prompt_file = fixture.workspace.join("prompt.txt");
    fs::write(&prompt_file, "file prompt").expect("prompt file");
    let output_file = fixture.workspace.join("result.txt");
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("file output")));
    let mut args = run_args("ignored");
    args.positional_prompt = None;
    args.prompt_file = Some(prompt_file);
    args.output_file = Some(output_file.clone());
    let file = fixture.run(args, "").await;
    assert_eq!(file.code, 0, "{}", file.stderr);
    assert!(file.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(output_file).expect("output file"),
        "file output\n"
    );

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"printf tool-output"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("tool complete")));
    let mut args = run_args("tool prompt");
    args.output = Some(OutputMode::Json);
    args.verbose = true;
    args.allowed_tools = vec![AllowedTool::Bash];
    let tool = fixture.run(args, "").await;
    assert_eq!(tool.code, 0, "{}", tool.stderr);
    assert!(!tool.stderr.contains('\u{1b}'));
    assert!(
        tool.stderr
            .lines()
            .all(|line| line.starts_with("cookie run: "))
    );
    let records = parse_json_lines(&tool.stdout);
    assert_jsonl_structure(&records);
    assert!(records.iter().any(|record| record["type"] == "tool_output"));
    let tool_session: SessionId =
        serde_json::from_value(records.last().expect("tool summary")["session_id"].clone())
            .expect("tool session ID");
    let write = fixture
        .engine
        .get_session_permissions(tool_session)
        .expect("session permissions")
        .permissions
        .into_iter()
        .find(|permission| permission.action == PermissionAction::Write)
        .expect("write permission");
    assert_eq!(write.effect, PermissionEffect::Ask);

    fixture.shutdown().await;
}

#[tokio::test]
async fn headless_skill_is_permission_checked_and_injected_before_the_prompt() {
    let fixture = Fixture::new().await;
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("skill loaded")));
    let mut args = run_args("check with the skill");
    args.skill = Some("release-check".into());
    args.skill_args = Some("v1.2.0".into());
    args.allowed_tools = vec![AllowedTool::Skill("release-check".into())];
    let result = fixture.run(args, "").await;
    assert_eq!(result.code, 0, "{}", result.stderr);
    let request = fixture
        .server
        .requests()
        .into_iter()
        .next()
        .expect("model request");
    assert!(request.contains("Release v1.2.0 from v1.2.0"), "{request}");
    assert!(request.contains("check with the skill"), "{request}");
    fixture.shutdown().await;
}

#[tokio::test]
async fn model_disabled_skill_lookup_rejects_without_leaking_hidden_hints() {
    let fixture = Fixture::new().await;
    let session = fixture
        .engine
        .create_session(RunSelection {
            agent: "primary".parse().expect("agent"),
            model: cookie_agent_protocol::ModelSelection {
                model: "custom.local/test".parse().expect("model"),
                variant: None,
            },
            preset: None,
        })
        .expect("session");
    assert!(
        fixture
            .engine
            .get_skill(session.session_id, "hidden-model", "")
            .is_ok(),
        "user preview remains available"
    );
    let listed = fixture
        .engine
        .list_skills(session.session_id)
        .expect("skill list");
    let visibility = |name: &str| {
        listed
            .skills
            .iter()
            .find(|skill| skill.name == name && skill.precedence_winner)
            .map(|skill| skill.visible)
            .expect("listed skill")
    };
    assert!(visibility("release-check"));
    assert!(!visibility("hidden-model"));
    assert!(!visibility("denied-skill"));
    let hidden = fixture
        .engine
        .get_model_skill(session.session_id, "hidden-model", "")
        .unwrap_err()
        .to_string();
    let hidden_hints = hidden.split("valid skills:").nth(1).unwrap_or_default();
    assert!(hidden_hints.contains("release-check"), "{hidden}");
    assert!(!hidden_hints.contains("hidden-model"), "{hidden}");
    assert!(!hidden.contains("denied-skill"), "{hidden}");

    let denied = fixture
        .engine
        .get_model_skill(session.session_id, "denied-skill", "")
        .unwrap_err()
        .to_string();
    let hints = denied.split("valid skills:").nth(1).unwrap_or_default();
    assert!(!hints.contains("hidden-model"), "{denied}");
    assert!(!hints.contains("denied-skill"), "{denied}");
    fixture.shutdown().await;
}

#[tokio::test]
async fn direct_skill_ask_routes_through_headless_auto_rejection() {
    let fixture = Fixture::new().await;
    let mut args = run_args("approval required");
    args.skill = Some("release-check".into());
    args.permission_mode = PermissionModeArg::Ask;
    args.output = Some(OutputMode::Json);
    let result = fixture.run(args, "").await;
    assert_eq!(result.code, 3, "{}\n{}", result.stderr, result.stdout);
    let records = parse_json_lines(&result.stdout);
    assert!(records.iter().any(|record| {
        record["type"] == "event" && record["event"]["payload"]["type"] == "approval_escalated"
    }));
    fixture.shutdown().await;
}

#[tokio::test]
async fn skill_grant_executes_hidden_bash_for_one_turn_only() {
    let fixture = Fixture::new().await;
    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"git --version"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("granted")));
    let mut first = run_args("use the grant");
    first.agent = Some("skill-host".parse().expect("agent"));
    first.skill = Some("release-check".into());
    first.output = Some(OutputMode::Json);
    let first = fixture.run(first, "").await;
    assert_eq!(first.code, 0, "{}", first.stderr);
    let first_records = parse_json_lines(&first.stdout);
    let session_id: SessionId =
        serde_json::from_value(first_records.last().expect("summary")["session_id"].clone())
            .expect("session ID");
    let first_run: cookie_agent_protocol::RunId =
        serde_json::from_value(first_records.last().expect("summary")["run_id"].clone())
            .expect("run ID");
    assert!(first_records.iter().any(|record| {
        record["type"] == "event"
            && record["event"]["payload"]["type"] == "tool_call_terminated"
            && record["event"]["payload"]["outcome"] == "completed"
    }));
    assert!(
        fixture
            .engine
            .steer(first_run, "unconfirmed steer".into())
            .await
            .is_err()
    );
    let failed_start = fixture
        .engine
        .start_run(RunStartParams {
            session_id,
            client_run_id: ClientRunId::new(uuid::Uuid::now_v7().to_string())
                .expect("client run ID"),
            selection: RunSelection {
                agent: "missing-agent".parse().expect("agent"),
                model: cookie_agent_protocol::ModelSelection {
                    model: "custom.local/test".parse().expect("model"),
                    variant: None,
                },
                preset: None,
            },
            input: cookie_agent_protocol::encode_skill_submission_with_prompt(
                "release-check",
                "failed",
                Some("must not install"),
            ),
        })
        .await;
    assert!(failed_start.is_err());

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"git init leaked-repo"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("not granted")));
    let mut second = run_args("next user turn");
    second.resume_session = Some(session_id);
    let second = fixture.run(second, "").await;
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert!(!fixture.workspace.join("leaked-repo").exists());
    fixture.shutdown().await;
}

#[tokio::test]
async fn fork_skill_uses_delegate_approval_and_installs_only_in_child() {
    let fixture = Fixture::new().await;
    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "skill",
        r#"{"name":"fork-skill","args":""}"#,
    )));
    let mut denied = run_args("fork requires approval");
    denied.agent = Some("skill-host".parse().expect("agent"));
    denied.permission_mode = PermissionModeArg::Ask;
    denied.output = Some(OutputMode::Json);
    let denied = fixture.run(denied, "").await;
    assert_eq!(denied.code, 3, "{}", denied.stderr);
    assert!(parse_json_lines(&denied.stdout).iter().any(|record| {
        record["type"] == "event" && record["event"]["payload"]["type"] == "approval_escalated"
    }));

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "skill",
        r#"{"name":"fork-skill","args":""}"#,
    )));
    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"git --version"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("child done")));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("parent done")));
    let mut allowed = run_args("fork with approval grant");
    allowed.agent = Some("skill-host".parse().expect("agent"));
    allowed.allowed_tools = vec![AllowedTool::Delegate];
    allowed.output = Some(OutputMode::Json);
    let allowed = fixture.run(allowed, "").await;
    assert_eq!(allowed.code, 0, "{}", allowed.stderr);
    let records = parse_json_lines(&allowed.stdout);
    let parent: SessionId =
        serde_json::from_value(records.last().expect("summary")["session_id"].clone())
            .expect("parent session");
    let child = fixture
        .engine
        .children(parent)
        .into_iter()
        .next()
        .unwrap_or_else(|| {
            panic!(
                "fork child missing; output={} requests={:?}",
                allowed.stdout,
                fixture.server.requests()
            )
        })
        .session_id;
    let child_history = serde_json::to_string(
        &fixture
            .engine
            .get_history(child, EngineHistoryView::Assembled)
            .await
            .expect("child history"),
    )
    .expect("child history JSON");
    assert!(
        child_history.contains("Forked skill body"),
        "{child_history}"
    );
    let body_requests = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.contains("Forked skill body"))
        .collect::<Vec<_>>();
    assert!(
        !body_requests.is_empty(),
        "child request omitted skill body"
    );
    for request in body_requests {
        assert_eq!(request.matches("Forked skill body").count(), 1, "{request}");
    }
    let parent_history = serde_json::to_string(
        &fixture
            .engine
            .get_history(parent, EngineHistoryView::Assembled)
            .await
            .expect("parent history"),
    )
    .expect("parent history JSON");
    assert!(
        !parent_history.contains("Forked skill body"),
        "{parent_history}"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn direct_fork_skill_uses_prepared_delegate_path() {
    let fixture = Fixture::new().await;
    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"git --version"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("child direct done")));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("parent direct done")));
    let mut args = run_args("direct fork");
    args.agent = Some("skill-host".parse().expect("agent"));
    args.skill = Some("fork-skill".into());
    args.allowed_tools = vec![AllowedTool::Delegate];
    args.output = Some(OutputMode::Json);
    let result = fixture.run(args, "").await;
    assert_eq!(result.code, 0, "{}", result.stderr);
    let records = parse_json_lines(&result.stdout);
    let parent: SessionId =
        serde_json::from_value(records.last().expect("summary")["session_id"].clone())
            .expect("parent");
    let child = fixture
        .engine
        .children(parent)
        .into_iter()
        .next()
        .expect("direct fork child");
    let history = serde_json::to_string(
        &fixture
            .engine
            .get_history(child.session_id, EngineHistoryView::Assembled)
            .await
            .expect("child history"),
    )
    .expect("history JSON");
    assert!(history.contains("Forked skill body"), "{history}");
    let body_requests = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.contains("Forked skill body"))
        .collect::<Vec<_>>();
    assert!(
        !body_requests.is_empty(),
        "child request omitted skill body"
    );
    for request in body_requests {
        assert_eq!(request.matches("Forked skill body").count(), 1, "{request}");
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn prospective_chained_skill_listing_preserves_a_grants_after_b_loads() {
    let fixture = Fixture::new().await;
    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "skill",
        r#"{"name":"grant-b","args":""}"#,
    )));
    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"git --version"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("chain complete")));
    let mut args = run_args("chain skills");
    args.agent = Some("skill-host".parse().expect("agent"));
    args.skill = Some("grant-a".into());
    let result = fixture.run(args, "").await;
    assert_eq!(result.code, 0, "{}", result.stderr);
    let requests = fixture.server.requests();
    assert!(
        requests[0].contains("<name>grant-b</name>"),
        "{}",
        requests[0]
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("git --version"))
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_events_determine_failure_permission_and_cancellation_codes() {
    let fixture = Fixture::new().await;

    for _ in 0..10 {
        fixture.server.enqueue(MockResponse::Status(500));
    }
    let failed = fixture.run(run_args("failure prompt"), "").await;
    assert_eq!(failed.code, 1, "{}", failed.stderr);

    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("terminal won race")));
    let mut race_args = run_args("terminal race prompt");
    race_args.output = Some(OutputMode::Json);
    race_args.max_turns = 1;
    let mut race_input = Cursor::new(Vec::<u8>::new());
    let mut race_output = SlowLineWriter::default();
    let mut race_stderr = Vec::new();
    let race_code = execute_with_io(
        &fixture.engine,
        race_args,
        &mut race_input,
        &mut race_output,
        &mut race_stderr,
        pending(),
    )
    .await;
    assert_eq!(race_code, 0, "{}", String::from_utf8_lossy(&race_stderr));
    let race_records =
        parse_json_lines(&String::from_utf8(race_output.bytes).expect("race output"));
    assert!(race_records.last().unwrap()["cancellation"].is_null());

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"true"}"#,
    )));
    let mut permission_args = run_args("permission prompt");
    permission_args.permission_mode = PermissionModeArg::Ask;
    permission_args.output = Some(OutputMode::Json);
    let permission = fixture.run(permission_args, "").await;
    assert_eq!(permission.code, 3, "{}", permission.stderr);
    let permission_summary = parse_json_lines(&permission.stdout)
        .pop()
        .expect("permission summary");
    assert_eq!(permission_summary["status"], "cancelled");
    assert_eq!(permission_summary["cancellation"], "permission");

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"true"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response(r#"{"decision":"ask"}"#)));
    let mut auto_n_args = run_args("auto-n permission prompt");
    auto_n_args.permission_mode = PermissionModeArg::AutoApproveN;
    auto_n_args.output = Some(OutputMode::Json);
    let auto_n = fixture.run(auto_n_args, "").await;
    assert_eq!(auto_n.code, 3, "{}", auto_n.stderr);
    let auto_n_summary = parse_json_lines(&auto_n.stdout)
        .pop()
        .expect("auto-n permission summary");
    assert_eq!(auto_n_summary["status"], "cancelled");
    assert_eq!(auto_n_summary["cancellation"], "permission");

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"true"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response(r#"{"decision":"ask"}"#)));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("auto-y continued")));
    let mut auto_y_args = run_args("auto-y permission prompt");
    auto_y_args.permission_mode = PermissionModeArg::AutoApproveY;
    auto_y_args.output = Some(OutputMode::Json);
    let auto_y = fixture.run(auto_y_args, "").await;
    assert_eq!(auto_y.code, 0, "{}", auto_y.stderr);
    let auto_y_summary = parse_json_lines(&auto_y.stdout)
        .pop()
        .expect("auto-y completion summary");
    assert_eq!(auto_y_summary["status"], "completed");
    assert!(auto_y_summary["cancellation"].is_null());
    assert_eq!(auto_y_summary["final_text"], "auto-y continued");

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"true"}"#,
    )));
    let mut turns_args = run_args("turn cap prompt");
    turns_args.max_turns = 1;
    turns_args.allowed_tools = vec![AllowedTool::Bash];
    let turns = fixture.run(turns_args, "").await;
    assert_eq!(turns.code, 4, "{}", turns.stderr);

    fixture
        .server
        .enqueue(MockResponse::Delay(Duration::from_millis(250)));
    let interrupted = fixture
        .run_with_interrupt(
            run_args("interrupt prompt"),
            "",
            tokio::time::sleep(Duration::from_millis(20)),
        )
        .await;
    assert_eq!(interrupted.code, 4, "{}", interrupted.stderr);

    fixture
        .server
        .enqueue(MockResponse::Delay(Duration::from_millis(1200)));
    let mut timeout_args = run_args("timeout prompt");
    timeout_args.timeout = 1;
    let timeout = fixture.run(timeout_args, "").await;
    assert_eq!(timeout.code, 4, "{}", timeout.stderr);

    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_gap_recovery_and_delegated_approval_are_hermetic() {
    let fixture = Fixture::new().await;

    let mut environment_args = run_args("ignored");
    environment_args.positional_prompt = None;
    environment_args.prompt_file = Some(fixture.workspace.join("missing-prompt.txt"));
    let environment = fixture.run(environment_args, "").await;
    assert_eq!(environment.code, 5);
    assert!(environment.stdout.is_empty());
    assert!(environment.stderr.contains("read prompt file"));

    let mut invalid_selection = run_args("invalid model prompt");
    invalid_selection.model = Some("custom.local/missing".parse().expect("model key"));
    let invalid_selection = fixture.run(invalid_selection, "").await;
    assert_eq!(invalid_selection.code, 5);
    assert!(invalid_selection.stderr.contains("is not available"));

    let gap_session = fixture
        .engine
        .create_session(RunSelection {
            agent: "primary".parse().expect("agent"),
            model: cookie_agent_protocol::ModelSelection {
                model: "custom.local/test".parse().expect("model"),
                variant: None,
            },
            preset: None,
        })
        .expect("gap session");
    let coordination_deadline = Instant::now() + Duration::from_secs(30);
    let (start_burst, await_burst) = std_mpsc::sync_channel(1);
    fixture.server.enqueue(MockResponse::GatedSse {
        body: burst_response(1500),
        start: await_burst,
        deadline: coordination_deadline,
    });
    let mut gap_args = run_args("gap prompt");
    gap_args.output = Some(OutputMode::Json);
    gap_args.resume_session = Some(gap_session.session_id);
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut gap_output = GatedLineWriter::new(
        start_burst,
        terminal_signal(
            &fixture.engine,
            gap_session.session_id,
            coordination_deadline,
        ),
        coordination_deadline,
    );
    let mut gap_stderr = Vec::new();
    let gap_code = execute_with_io(
        &fixture.engine,
        gap_args,
        &mut input,
        &mut gap_output,
        &mut gap_stderr,
        pending(),
    )
    .await;
    assert_eq!(gap_code, 0, "{}", String::from_utf8_lossy(&gap_stderr));
    let gap_records = parse_json_lines(&String::from_utf8(gap_output.bytes).expect("gap output"));
    assert_jsonl_structure(&gap_records);
    let gap_summary = gap_records.last().expect("gap summary");
    assert!(
        gap_summary["event_recoveries"]
            .as_u64()
            .expect("recovery count")
            > 0,
        "forced event burst did not exercise recovery ({} records)",
        gap_records.len()
    );

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "delegate_subagent",
        r#"{"agent_type":"reviewer","description":"Review","prompt":"review delegated work"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("initial child review")));
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("parent accepted review")));
    let mut parent_args = run_args("delegate prompt");
    parent_args.output = Some(OutputMode::Json);
    let parent = fixture.run(parent_args, "").await;
    assert_eq!(parent.code, 0, "{}", parent.stderr);
    let parent_records = parse_json_lines(&parent.stdout);
    let child_session: SessionId = parent_records
        .iter()
        .find_map(|record| {
            (record["type"] == "event" && record["event"]["payload"]["type"] == "tool_call_linked")
                .then(|| {
                    serde_json::from_value(record["event"]["payload"]["child_session_id"].clone())
                        .expect("child session ID")
                })
        })
        .expect("delegated child session");

    fixture.server.enqueue(MockResponse::Sse(tool_response(
        "bash",
        r#"{"command":"true"}"#,
    )));
    fixture
        .server
        .enqueue(MockResponse::Delay(Duration::from_millis(250)));
    let mut delegated_args = run_args("resumed child approval prompt");
    delegated_args.resume_session = Some(child_session);
    delegated_args.permission_mode = PermissionModeArg::Ask;
    delegated_args.output = Some(OutputMode::Json);
    let delegated = fixture.run(delegated_args, "").await;
    assert_eq!(delegated.code, 3, "{}", delegated.stderr);
    let delegated_records = parse_json_lines(&delegated.stdout);
    let delegated_summary = delegated_records.last().expect("resumed child summary");
    assert_eq!(delegated_summary["approval_rejections"], 1);
    assert_eq!(delegated_summary["cancellation"], "permission");

    fixture.shutdown().await;
}

#[tokio::test]
async fn start_run_admission_failures_map_to_one() {
    let fixture = Fixture::new().await;
    let snapshot = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot;
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == "primary")
        .expect("primary agent");
    let model = agent
        .resolved_fallback
        .iter()
        .find(|selection| {
            snapshot
                .models
                .iter()
                .any(|model| model.key == selection.model)
        })
        .expect("live model")
        .clone();
    let session = fixture
        .engine
        .create_session(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: None,
        })
        .expect("running session");
    fixture
        .server
        .enqueue(MockResponse::Delay(Duration::from_millis(500)));
    let running = fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("already-running").expect("client run ID"),
            selection: session.creation_selection,
            input: "keep running".into(),
        })
        .await
        .expect("active run");
    let mut args = run_args("conflicting run");
    args.resume_session = Some(session.session_id);
    let result = fixture.run(args, "").await;
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("already running"),
        "{}",
        result.stderr
    );
    let _ = fixture.engine.cancel_run(running.run_id).await;
    fixture.shutdown().await;
}

#[test]
fn cookie_binary_composes_runs_and_emits_json() {
    let fixture = ProcessFixture::new();
    fixture
        .server
        .enqueue(MockResponse::Sse(final_response("binary complete")));
    let output = fixture.run(&["run", "binary prompt", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", process_report(&output));
    assert!(output.stderr.is_empty(), "{}", process_report(&output));
    let stdout = String::from_utf8(output.stdout).expect("binary JSON output");
    let records = parse_json_lines(&stdout);
    assert_jsonl_structure(&records);
    assert_eq!(records.last().unwrap()["final_text"], "binary complete");
}

#[test]
fn cookie_binary_maps_run_and_environment_failures() {
    let fixture = ProcessFixture::new();
    for _ in 0..10 {
        fixture.server.enqueue(MockResponse::Status(500));
    }
    let failed = fixture.run(&["run", "binary failure", "--output", "none"]);
    assert_eq!(failed.status.code(), Some(1), "{}", process_report(&failed));

    let environment = fixture.run(&["run", "-f", "missing-prompt.txt", "--output", "none"]);
    assert_eq!(
        environment.status.code(),
        Some(5),
        "{}",
        process_report(&environment)
    );
    assert!(
        String::from_utf8_lossy(&environment.stderr).contains("read prompt file"),
        "{}",
        process_report(&environment)
    );

    let clap = fixture
        .command()
        .args([
            "run",
            "prompt",
            "--output",
            "none",
            "--output-file",
            "result.txt",
        ])
        .output()
        .expect("run invalid cookie CLI");
    assert_eq!(clap.status.code(), Some(2), "{}", process_report(&clap));
}

#[test]
fn cookie_binary_treats_plugin_handled_input_as_success_without_a_run() {
    let fixture = ProcessFixture::new();
    let config = fixture.workspace.join(".cookie-agent/config.toml");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(config)
        .expect("append plugin config");
    writeln!(
        file,
        r#"
[plugins.command_handler]
command = "/usr/bin/python3"
args = ['{PLUGIN_FIXTURE}']
env = {{ FIXTURE_NAME = 'command_handler', FIXTURE_CAPABILITIES = '{{"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["user_before_input"]}}', FIXTURE_USER_BEFORE_INPUT_RESULT = '{{"action":"handled","reason":"command consumed"}}' }}
"#
    )
    .expect("plugin config");

    let output = fixture.run(&["run", "/handled", "--output", "none"]);
    assert_eq!(output.status.code(), Some(0), "{}", process_report(&output));
    assert!(output.stdout.is_empty(), "{}", process_report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("command consumed"),
        "{}",
        process_report(&output)
    );
    assert!(
        stderr.contains("no run started"),
        "{}",
        process_report(&output)
    );
    assert!(fixture.server.requests().is_empty());
}

#[test]
fn cookie_binary_treats_blocked_model_selection_as_failure() {
    let fixture = ProcessFixture::new();
    let config = fixture.workspace.join(".cookie-agent/config.toml");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(config)
        .expect("append plugin config");
    writeln!(
        file,
        r#"
[plugins.model_guard]
command = "/usr/bin/python3"
args = ['{PLUGIN_FIXTURE}']
env = {{ FIXTURE_NAME = 'model_guard', FIXTURE_CAPABILITIES = '{{"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["model_before_select"]}}', FIXTURE_MODEL_BEFORE_SELECT_RESULT = '{{"action":"block","reason":"model denied"}}' }}
"#
    )
    .expect("plugin config");

    let output = fixture.run(&["run", "blocked model", "--output", "none"]);
    assert_eq!(output.status.code(), Some(1), "{}", process_report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("model denied"),
        "{}",
        process_report(&output)
    );
    assert!(
        !stderr.contains("no run started"),
        "{}",
        process_report(&output)
    );
    assert!(fixture.server.requests().is_empty());
}

#[test]
fn cookie_binary_sigint_cancels_the_active_run() {
    let fixture = ProcessFixture::new();
    fixture
        .server
        .enqueue(MockResponse::Delay(Duration::from_millis(500)));
    let child = fixture
        .command()
        .args(["run", "SIGINT prompt", "--output", "none", "--data-dir"])
        .arg(&fixture.data_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cookie SIGINT run");
    for _ in 0..1_000 {
        if !fixture.server.requests().is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !fixture.server.requests().is_empty(),
        "cookie binary did not start its model request"
    );
    let result = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(result, 0, "send SIGINT to cookie binary");
    let output = child
        .wait_with_output()
        .expect("wait for cookie SIGINT run");
    assert_eq!(output.status.code(), Some(4), "{}", process_report(&output));
    assert!(output.stderr.is_empty(), "{}", process_report(&output));
}

fn run_args(prompt: &str) -> RunArgs {
    RunArgs {
        positional_prompt: Some(prompt.into()),
        prompt: None,
        prompt_file: None,
        agent: None,
        model: None,
        variant: None,
        permission_mode: PermissionModeArg::AutoApprove,
        allowed_tools: Vec::new(),
        skill: None,
        skill_args: None,
        max_turns: 100,
        timeout: 10,
        resume_session: None,
        data_dir: None,
        output: Some(OutputMode::Text),
        output_file: None,
        verbose: false,
        json: false,
    }
}

fn process_report(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn parse_json_lines(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL record"))
        .collect()
}

fn assert_jsonl_structure(records: &[serde_json::Value]) {
    assert!(!records.is_empty());
    for (index, record) in records.iter().enumerate() {
        let kind = record["type"].as_str().expect("record type");
        match kind {
            "event" => {
                assert!(record["event"].is_object());
                assert!(record["event"]["session_id"].is_string());
                assert!(record["event"]["seq"].is_u64());
                assert!(record["event"]["payload"]["type"].is_string());
            }
            "tool_output" => {
                assert!(record["call_id"].is_string());
                assert!(record["stream"].is_string());
                assert!(record["byte_offset"].is_u64());
                assert!(record["data"].is_string());
            }
            "tool_output_gap" => {
                assert!(record["call_id"].is_string());
                assert!(record["stream"].is_string());
                assert!(record["next_offset"].is_u64());
            }
            "summary" => {
                assert_eq!(index, records.len() - 1, "summary must be final");
                for field in [
                    "session_id",
                    "run_id",
                    "status",
                    "exit_code",
                    "turns",
                    "approval_rejections",
                    "event_recoveries",
                    "cancellation",
                    "final_text",
                    "usage",
                ] {
                    assert!(record.get(field).is_some(), "missing summary field {field}");
                }
                assert!(record["usage"]["by_model"].is_object());
            }
            other => panic!("unknown JSONL record type {other}"),
        }
    }
    assert_eq!(records.last().unwrap()["type"], "summary");
}

fn write_workspace(workspace: &Path, endpoint: &str) {
    let root = workspace.join(".cookie-agent");
    let agents = root.join("agents");
    let skills = root.join("skills");
    fs::create_dir_all(&agents).expect("agent directory");
    for skill in [
        "release-check",
        "hidden-model",
        "denied-skill",
        "fork-skill",
        "grant-a",
        "grant-b",
    ] {
        fs::create_dir_all(skills.join(skill)).expect("skill directory");
    }
    make_private(&root);
    make_private(&agents);
    make_private(&skills);
    for skill in [
        "release-check",
        "hidden-model",
        "denied-skill",
        "fork-skill",
        "grant-a",
        "grant-b",
    ] {
        make_private(&skills.join(skill));
    }
    fs::write(
        skills.join("release-check/SKILL.md"),
        r#"---
name: release-check
description: Check a release
allowed-tools: Bash(git:*) Read
---
Release $1 from $ARGUMENTS.
"#,
    )
    .expect("release skill");
    fs::write(
        skills.join("hidden-model/SKILL.md"),
        r#"---
name: hidden-model
description: User-only hidden skill
disable-model-invocation: true
---
Hidden body.
"#,
    )
    .expect("hidden model skill");
    fs::write(
        skills.join("denied-skill/SKILL.md"),
        r#"---
name: denied-skill
description: Denied skill
---
Denied body.
"#,
    )
    .expect("denied skill");
    fs::write(
        skills.join("fork-skill/SKILL.md"),
        r#"---
name: fork-skill
description: Fork this skill
allowed-tools: Bash(git:*)
context: fork
---
Forked skill body.
"#,
    )
    .expect("fork skill");
    fs::write(
        skills.join("grant-a/SKILL.md"),
        r#"---
name: grant-a
description: Grant another skill and bash
allowed-tools: Skill(grant-b) Bash(git:*)
---
Grant A body.
"#,
    )
    .expect("grant A skill");
    fs::write(
        skills.join("grant-b/SKILL.md"),
        r#"---
name: grant-b
description: Grantless second skill
---
Grant B body.
"#,
    )
    .expect("grant B skill");
    fs::write(
        root.join("config.toml"),
        format!(
            r#"[session_title]
generate_on_first_turn = false

[providers."custom.local"]
source = "custom"
endpoint = "{endpoint}"
adaptor = "openai-compatible"
auth = {{ method = "no-auth-v1", values = {{}} }}

[providers."custom.local".models.test]
display_name = "Headless Test"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {{}} }}

[providers."custom.local".models.alternate]
display_name = "Headless Alternate"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {{}} }}
"#,
        ),
    )
    .expect("workspace config");
    fs::write(
        agents.join("primary.md"),
        r#"---
description: Headless integration agent
mode: primary
enabled: true
models:
  - { model: "custom.missing/unavailable", variant: null }
  - { model: "custom.local/test", variant: null }
  - { model: "custom.local/alternate", variant: null }
permissions:
  read: ask
  write: ask
  bash: ask
  skill:
    "*": ask
    denied-skill: deny
  delegate:
    reviewer: allow
---
Answer the user directly.
"#,
    )
    .expect("primary agent");
    fs::write(
        agents.join("reviewer.md"),
        r#"---
description: Delegated review agent
mode: subagent
enabled: true
models:
  - { model: "custom.local/test", variant: null }
permissions:
  bash: ask
---
Review delegated work.
"#,
    )
    .expect("reviewer agent");
    fs::write(
        agents.join("skill-host.md"),
        r#"---
description: Skill grant integration agent
mode: primary
enabled: true
models:
  - { model: "custom.local/test", variant: null }
permissions:
  skill:
    release-check: allow
    fork-skill: allow
    grant-a: allow
  delegate:
    reviewer: ask
---
Use loaded skills.
"#,
    )
    .expect("skill host agent");
}

fn make_private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private directory");
}

fn final_response(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}}}\n\n",
        serde_json::to_string(text).expect("response text")
    )
}

fn burst_response(deltas: usize) -> String {
    let mut body = String::new();
    for _ in 0..deltas {
        body.push_str(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
        );
    }
    body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n");
    body
}

fn tool_response(name: &str, arguments: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"headless-call-{}\",\"type\":\"function\",\"function\":{{\"name\":{name},\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}}}\n\n",
        uuid::Uuid::now_v7(),
        name = serde_json::to_string(name).expect("tool name"),
        arguments = serde_json::to_string(arguments).expect("tool arguments"),
    )
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request timeout");
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("read model request");
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("read model request body");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Some(String::from_utf8(bytes).expect("UTF-8 request"))
}

fn write_sse(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write model response");
}

fn write_status(stream: &mut TcpStream, status: u16) {
    let response = format!(
        "HTTP/1.1 {status} Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    stream
        .write_all(response.as_bytes())
        .expect("write model failure");
}
