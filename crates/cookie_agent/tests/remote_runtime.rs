#![cfg(unix)]
//! A configured `[runtimes.<id>]` template whose command runs the real
//! `cookie runtime serve --stdio` binary, as an ssh template would on the
//! remote machine, and the same binary dialing in with
//! `cookie runtime serve --daemon`, driven by an in-process daemon and a
//! scripted model.

use std::{
    collections::VecDeque,
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use cookie_agent_engine::{
    Engine, EngineOptions, RuntimeInstanceStatus, SessionRuntimeRequest, SessionRuntimes,
};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogManager, CatalogTransport, CatalogTransportFuture, CatalogTransportResponse,
        MODELS_DEV_BOOTSTRAP,
    },
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, EventSubscriptionMessage, ModelSelection,
    PermissionMode, RunSelection, RunStartParams, SessionId,
};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(60);

/// A model endpoint that answers each request with the next queued SSE
/// body and records the request.
struct MockModel {
    address: std::net::SocketAddr,
    responses: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl MockModel {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock model");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("mock address");
        let responses = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (task_responses, task_requests, task_stop) = (
            Arc::clone(&responses),
            Arc::clone(&requests),
            Arc::clone(&stop),
        );
        let task = thread::spawn(move || {
            while !task_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(request) = read_request(&mut stream) else {
                            continue;
                        };
                        task_requests.lock().unwrap().push(request);
                        let Some(body) = task_responses.lock().unwrap().pop_front() else {
                            continue;
                        };
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
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

    fn enqueue(&self, body: String) {
        self.responses.lock().unwrap().push_back(body);
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockModel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request timeout");
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn tool_call(name: &str, arguments: &serde_json::Value) -> String {
    let call = serde_json::json!({"choices":[{"delta":{"tool_calls":[{
        "index": 0,
        "id": format!("call-{}", uuid::Uuid::now_v7()),
        "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()},
    }]},"finish_reason":null}]});
    format!(
        "data: {call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}}}\n\n"
    )
}

fn final_answer(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}}}\n\n",
        serde_json::Value::from(text)
    )
}

/// The tool names a model request offered.
fn offered_tools(request: &str) -> Vec<String> {
    let body = &request[request.find("\r\n\r\n").expect("request body") + 4..];
    let body: serde_json::Value = serde_json::from_str(body).expect("JSON request body");
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy)]
struct BundledCatalog;

impl CatalogTransport for BundledCatalog {
    fn fetch(&self, _etag: Option<String>) -> CatalogTransportFuture<'_> {
        Box::pin(async {
            Ok(CatalogTransportResponse::from_bytes(
                200,
                MODELS_DEV_BOOTSTRAP.to_vec(),
            ))
        })
    }
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private directory");
}

struct Fixture {
    root: TempDir,
    /// The daemon's working directory: the `local` runtime's workspace.
    web: PathBuf,
    /// The `HOME` the remote runtime sees.
    remote_home: PathBuf,
    /// `~/repo` on the "remote machine".
    repo: PathBuf,
    model: MockModel,
    engine: Engine,
}

impl Fixture {
    /// A daemon whose `remote` template runs `cookie runtime coding` through
    /// `env`, which passes its arguments on unchanged as `ssh` passes them
    /// to the remote command.
    async fn new(startup_timeout_ms: u64) -> Self {
        Self::with_config(startup_timeout_ms, "").await
    }

    /// [`Self::new`] with `extra` appended to `config.toml`.
    async fn with_config(startup_timeout_ms: u64, extra: &str) -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        private_dir(root.path());
        let web = root.path().join("web");
        let remote_home = root.path().join("remote-home");
        let repo = remote_home.join("repo");
        let config_root = root.path().join("config");
        for directory in [&web, &remote_home, &repo, &config_root] {
            private_dir(directory);
        }
        fs::write(repo.join("note.txt"), "remote note\n").expect("note");
        let model = MockModel::start();
        let command = serde_json::to_string(&[
            "env",
            "COOKIE_RUNTIME_VIA=env",
            env!("CARGO_BIN_EXE_cookie"),
            "runtime",
            "serve",
            "--stdio",
        ])
        .expect("command");
        fs::write(
            config_root.join("config.toml"),
            format!(
                r#"[session_title]
generate_on_first_turn = false

[providers."custom.local"]
source = "custom"
endpoint = "http://{address}/v1"
adaptor = "openai-compatible"
auth = {{ method = "no-auth-v1", values = {{}} }}

[providers."custom.local".models.test]
display_name = "Remote Runtime Test"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {{}} }}

[runtimes.remote]
command = {command}
env = {{ HOME = {home} }}
startup_timeout_ms = {startup_timeout_ms}
shutdown_grace_ms = 2000
{extra}"#,
                address = model.address,
                home = serde_json::Value::from(remote_home.to_string_lossy().into_owned()),
            ),
        )
        .expect("config");
        let agents = config_root.join("agents");
        private_dir(&agents);
        fs::write(
            agents.join("primary.md"),
            "---\ndescription: Remote runtime agent\nmode: primary\nenabled: true\nmodels:\n  - { model: \"custom.local/test\", variant: null }\npermissions:\n  read: allow\n  bash: allow\n  mcp: allow\n---\nUse the tools.\n",
        )
        .expect("agent");
        let configuration =
            cookie_agent_config::load_from_user_root(Some(&config_root)).expect("configuration");
        let catalog = Arc::new(
            CatalogManager::in_directory(BundledCatalog, root.path(), "catalog")
                .refresh()
                .await
                .expect("bundled catalog"),
        );
        let providers = root.path().join("providers");
        private_dir(&providers);
        let model_manager = Arc::new(
            ModelManager::new_with_headers(
                configuration.runtime.providers.clone(),
                configuration.runtime.headers.clone(),
                catalog,
                ProviderStore::open(&providers).expect("provider store"),
            )
            .expect("model manager"),
        );
        let engine = Engine::open_async(EngineOptions {
            data_dir: root.path().join("data"),
            cwd: web.clone(),
            config: configuration,
            model_manager,
            runtimes: cookie_agent_coding_runtime::runtime_options(&web),
        })
        .await
        .expect("engine");
        Self {
            root,
            web,
            remote_home,
            repo,
            model,
            engine,
        }
    }

    fn selection() -> RunSelection {
        RunSelection {
            agent: "primary".parse().expect("agent"),
            model: ModelSelection {
                model: "custom.local/test".parse().expect("model"),
                variant: None,
            },
            preset: None,
        }
    }

    /// A session attached to `global`, `local`, and `remote` at `~/repo`.
    async fn session(&self) -> SessionId {
        self.engine
            .create_session_with_permissions(
                Self::selection(),
                Some(PermissionMode::Yolo),
                Vec::new(),
                SessionRuntimes {
                    runtimes: Some(vec![
                        SessionRuntimeRequest {
                            runtime_id: "local".into(),
                            capability: None,
                            workspace: None,
                        },
                        SessionRuntimeRequest {
                            runtime_id: "remote".into(),
                            capability: None,
                            workspace: Some("~/repo".into()),
                        },
                    ]),
                    global: true,
                    cwd: None,
                },
            )
            .await
            .expect("session")
            .session_id
    }

    /// Runs one prompt to its end.
    async fn run(&self, session: SessionId, prompt: &str) -> EventPayload {
        let (_, mut events) = self.engine.subscribe(session, None).await.expect("events");
        let run = self
            .engine
            .start_run(
                RunStartParams {
                    session_id: session,
                    client_run_id: ClientRunId::new(uuid::Uuid::now_v7().to_string())
                        .expect("client run ID"),
                    selection: Self::selection(),
                    content: prompt.into(),
                    reset_fallback: false,
                },
                EventOrigin::new("client:test").expect("origin"),
            )
            .await
            .expect("run starts")
            .run_id;
        tokio::time::timeout(WAIT, async {
            while let Some(message) = events.recv().await {
                if let EventSubscriptionMessage::Event { event } = message
                    && event.run_id == Some(run)
                    && matches!(event.payload, EventPayload::RunFinished { .. })
                {
                    return event.payload.clone();
                }
            }
            panic!("the event stream ended before the run did");
        })
        .await
        .expect("the run ends")
    }

    fn remote_status(&self) -> Option<RuntimeInstanceStatus> {
        self.engine
            .runtime_instances()
            .into_iter()
            .find(|instance| instance.runtime_id == "remote")
            .map(|instance| instance.status)
    }

    async fn wait_for_remote(&self, status: RuntimeInstanceStatus) {
        tokio::time::timeout(WAIT, async {
            while self.remote_status() != Some(status) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("`remote` never became {status:?}"));
    }

    /// Runs `bash` on the remote runtime to record its process ID in
    /// `file` (relative to the remote workspace) and returns it.
    async fn remote_pid(&self, session: SessionId, file: &str) -> i32 {
        self.model.enqueue(tool_call(
            "remote_repo_bash",
            &serde_json::json!({"command": format!("echo $PPID > {file}")}),
        ));
        self.model.enqueue(final_answer("recorded"));
        let ended = self.run(session, "record the runtime process").await;
        assert!(
            matches!(
                ended,
                EventPayload::RunFinished {
                    outcome: cookie_agent_protocol::Open::Known(
                        cookie_agent_protocol::RunOutcome::Completed { .. }
                    )
                }
            ),
            "{ended:?}"
        );
        fs::read_to_string(self.repo.join(file))
            .expect("the bash call ran in the remote workspace")
            .trim()
            .parse()
            .expect("process ID")
    }
}

fn kill(pid: i32) {
    // SAFETY: plain signal delivery to a child process this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0, "kill {pid}");
}

#[tokio::test]
async fn a_configured_runtime_runs_tools_in_its_workspace_next_to_local() {
    let fixture = Fixture::new(30_000).await;
    let templates = fixture.engine.runtime_templates();
    assert!(
        templates
            .iter()
            .any(|template| template.runtime_id == "remote"
                && template.lifetime == cookie_agent_protocol::RuntimeLifetime::Spawned
                && !template.listing.connected),
        "{templates:?}"
    );
    let session = fixture.session().await;

    // The runtime resolved `~/repo` on its machine (its HOME came from the
    // template's env); the session records the canonical path.
    let canonical = fixture.repo.canonicalize().unwrap();
    let attachments = fixture.engine.session_runtimes(session).unwrap();
    let remote = attachments
        .iter()
        .find(|attachment| attachment.runtime_id.as_str() == "remote")
        .expect("remote attachment");
    assert_eq!(remote.name.as_str(), "remote_repo");
    assert_eq!(
        remote.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    let local = attachments
        .iter()
        .find(|attachment| attachment.runtime_id.as_str() == "local")
        .expect("local attachment");
    assert_eq!(local.name.as_str(), "local_web");

    fixture.model.enqueue(tool_call(
        "remote_repo_bash",
        &serde_json::json!({"command": "pwd > where.txt"}),
    ));
    fixture.model.enqueue(tool_call(
        "remote_repo_read",
        &serde_json::json!({"filePath": "note.txt"}),
    ));
    fixture.model.enqueue(final_answer("done"));
    let ended = fixture.run(session, "look at the remote repository").await;
    assert!(
        matches!(
            ended,
            EventPayload::RunFinished {
                outcome: cookie_agent_protocol::Open::Known(
                    cookie_agent_protocol::RunOutcome::Completed { .. }
                )
            }
        ),
        "{ended:?}"
    );

    // Both coding runtimes offer the same tools, so each is prefixed with
    // its attachment name (the agent may use only `read` and `bash`).
    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    let tools = offered_tools(&requests[0]);
    for name in [
        "local_web_bash",
        "local_web_read",
        "remote_repo_bash",
        "remote_repo_read",
    ] {
        assert!(tools.iter().any(|tool| tool == name), "{name}: {tools:?}");
    }
    assert!(!tools.iter().any(|tool| tool == "bash"), "{tools:?}");
    // `bash` ran in the remote workspace, and `read` read from it.
    assert_eq!(
        fs::read_to_string(fixture.repo.join("where.txt"))
            .unwrap()
            .trim(),
        canonical.to_string_lossy()
    );
    assert!(!fixture.web.join("where.txt").exists());
    assert!(requests[2].contains("remote note"), "{}", requests[2]);

    // Another spelling of the same workspace shares the instance.
    let instance = fixture
        .engine
        .reconnect_runtime("remote", None, Some(canonical.clone()))
        .await
        .unwrap();
    assert_eq!(instance.status, RuntimeInstanceStatus::Ready);
    let remote_instances = fixture
        .engine
        .runtime_instances()
        .into_iter()
        .filter(|instance| instance.runtime_id == "remote")
        .count();
    assert_eq!(remote_instances, 1);
    fixture.engine.shutdown().await;
    drop(fixture.root);
}

#[tokio::test]
async fn a_killed_runtime_reconnects_and_one_that_cannot_is_dropped() {
    let fixture = Fixture::new(30_000).await;
    let session = fixture.session().await;
    let mut failures = fixture.engine.subscribe_runtime_failures();
    let first = fixture.remote_pid(session, "pid-1").await;

    // A crash: the instance reconnects with its tools published meanwhile.
    kill(first);
    fixture
        .wait_for_remote(RuntimeInstanceStatus::Reconnecting)
        .await;
    fixture.wait_for_remote(RuntimeInstanceStatus::Ready).await;
    let second = fixture.remote_pid(session, "pid-2").await;
    assert_ne!(first, second, "a new runtime process");
    assert!(failures.try_recv().is_err(), "a reconnect is no failure");

    // A crash the runtime cannot recover from: its workspace is gone, so
    // every restart fails (1 s, 2 s, 4 s), its tools are dropped, and
    // clients are told why, with the runtime's stderr.
    let moved = fixture.remote_home.join("moved");
    fs::rename(&fixture.repo, &moved).unwrap();
    kill(second);
    let failure = tokio::time::timeout(WAIT, failures.recv())
        .await
        .expect("the failure is reported")
        .expect("failure");
    assert_eq!(failure.runtime_id, "remote");
    assert!(
        failure.reason.contains("reconnecting failed 3 times"),
        "{}",
        failure.reason
    );
    assert!(
        failure.reason.contains("workspace") && failure.reason.contains("unavailable"),
        "the runtime's stderr: {}",
        failure.reason
    );
    assert_eq!(fixture.remote_status(), Some(RuntimeInstanceStatus::Failed));

    // Without the remote runtime nothing collides: `bash` is `local`'s.
    fixture.model.enqueue(final_answer("only local"));
    let before = fixture.model.requests().len();
    fixture.run(session, "what can you use?").await;
    let tools = offered_tools(&fixture.model.requests()[before]);
    assert!(tools.iter().any(|tool| tool == "bash"), "{tools:?}");
    assert!(
        !tools.iter().any(|tool| tool.starts_with("remote_repo_")),
        "{tools:?}"
    );

    // Retrying once the workspace is back restores the instance.
    fs::rename(&moved, &fixture.repo).unwrap();
    let canonical = fixture.repo.canonicalize().unwrap();
    let instance = fixture
        .engine
        .reconnect_runtime("remote", None, Some(canonical))
        .await
        .expect("retry");
    assert_eq!(instance.status, RuntimeInstanceStatus::Ready);
    let third = fixture.remote_pid(session, "pid-3").await;
    assert_ne!(third, second);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn failed_starts_name_the_reason_and_the_runtime_stderr() {
    let fixture = Fixture::new(30_000).await;
    let session = fixture
        .engine
        .create_session(Fixture::selection())
        .expect("session")
        .session_id;
    let mut failures = fixture.engine.subscribe_runtime_failures();
    let origin = EventOrigin::new("client:test").unwrap();

    // The runtime refuses a workspace that does not exist on its machine.
    let error = fixture
        .engine
        .attach_runtime(
            session,
            "remote",
            None,
            Some("~/missing".into()),
            origin.clone(),
        )
        .await
        .expect_err("no such workspace");
    let error = error.to_string();
    assert!(
        error.contains("could not open `coding` for workspace `~/missing`"),
        "{error}"
    );
    assert!(!error.contains("failed to start"), "{error}");
    // Nothing was attached or recorded: no session uses the instance.
    assert!(
        !fixture
            .engine
            .session_runtimes(session)
            .unwrap()
            .iter()
            .any(|attachment| attachment.runtime_id.as_str() == "remote")
    );
    assert_eq!(fixture.remote_status(), None);
    assert!(failures.try_recv().is_err());
    fixture.engine.shutdown().await;

    // A runtime that does not answer `runtime/initialize` in time.
    let fixture = Fixture::new(1).await;
    let session = fixture
        .engine
        .create_session(Fixture::selection())
        .expect("session")
        .session_id;
    let error = fixture
        .engine
        .attach_runtime(session, "remote", None, Some("~/repo".into()), origin)
        .await
        .expect_err("startup timeout")
        .to_string();
    assert!(error.contains("did not start within 1 ms"), "{error}");
    // No capability was named, so the runtime itself reports the failure.
    let remote = fixture
        .engine
        .runtime_templates()
        .into_iter()
        .find(|template| template.runtime_id == "remote")
        .expect("remote");
    assert!(!remote.listing.connected);
    assert!(
        remote
            .listing
            .message
            .as_deref()
            .is_some_and(|message| message.contains("did not start within 1 ms")),
        "{remote:?}"
    );
    fixture.engine.shutdown().await;
}

/// A runtime serving only some directories refuses a workspace outside
/// them: attaching it, creating a session with it, and retrying it say the
/// capability could not open there and give the runtime's reason, and
/// leave no instance behind. The runtime itself stays connected.
#[tokio::test]
async fn a_workspace_outside_the_runtime_roots_is_refused_and_records_nothing() {
    let served = tempfile::tempdir().expect("served directory");
    let root = served.path().to_string_lossy().into_owned();
    let command = serde_json::to_string(&[
        env!("CARGO_BIN_EXE_cookie"),
        "runtime",
        "serve",
        "--stdio",
        "--root",
        root.as_str(),
    ])
    .unwrap();
    let fixture = Fixture::with_config(
        30_000,
        &format!("\n[runtimes.rooted]\ncommand = {command}\nstartup_timeout_ms = 30000\n"),
    )
    .await;
    let outside = fixture.web.canonicalize().unwrap();
    let session = fixture
        .engine
        .create_session(Fixture::selection())
        .expect("session")
        .session_id;
    let mut failures = fixture.engine.subscribe_runtime_failures();
    let rooted_recorded = |engine: &Engine| {
        engine
            .runtime_instances()
            .into_iter()
            .any(|instance| instance.runtime_id == "rooted")
    };
    let refused = |error: String| {
        assert!(
            error.contains(&format!(
                "runtime `rooted` could not open `coding` for workspace `{}`",
                outside.display()
            )),
            "{error}"
        );
        assert!(
            error.contains("outside the directories this runtime serves"),
            "{error}"
        );
        assert!(!error.contains("failed to start"), "{error}");
    };

    let error = fixture
        .engine
        .attach_runtime(
            session,
            "rooted",
            None,
            Some(outside.clone()),
            EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect_err("outside the roots");
    refused(error.to_string());
    assert!(!rooted_recorded(&fixture.engine));
    assert!(
        !fixture
            .engine
            .session_runtimes(session)
            .unwrap()
            .iter()
            .any(|attachment| attachment.runtime_id.as_str() == "rooted")
    );
    // The runtime itself is up.
    let rooted = fixture
        .engine
        .runtime_templates()
        .into_iter()
        .find(|template| template.runtime_id == "rooted")
        .expect("rooted");
    assert!(rooted.listing.connected, "{rooted:?}");

    let error = fixture
        .engine
        .create_session_with_permissions(
            Fixture::selection(),
            None,
            Vec::new(),
            SessionRuntimes {
                runtimes: Some(vec![SessionRuntimeRequest {
                    runtime_id: "rooted".into(),
                    capability: Some("coding".into()),
                    workspace: Some(outside.clone()),
                }]),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect_err("a session outside the roots");
    refused(error.to_string());
    assert!(!rooted_recorded(&fixture.engine));

    let error = fixture
        .engine
        .reconnect_runtime("rooted", Some("coding"), Some(outside.clone()))
        .await
        .expect_err("a retry outside the roots");
    refused(error.to_string());
    assert!(!rooted_recorded(&fixture.engine));
    assert!(failures.try_recv().is_err());

    // A workspace under the roots still opens.
    fixture
        .engine
        .attach_runtime(
            session,
            "rooted",
            None,
            Some(served.path().to_owned()),
            EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("inside the roots");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_configured_runtime_hosts_its_template_mcp_servers() {
    let fixture = Fixture::with_config(
        30_000,
        &format!(
            "\n[runtimes.remote.mcp.servers.fixture]\ncommand = \"python3\"\nargs = [{}]\n",
            serde_json::Value::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../mcp/tests/fixtures/mcp_server.py"
            ))
        ),
    )
    .await;
    let session = fixture.session().await;

    // The runtime connected its server before the session was created.
    let canonical = fixture.repo.canonicalize().unwrap();
    let servers = fixture.engine.list_mcp_servers();
    let hosted = servers
        .iter()
        .find(|server| server.runtime_id.as_str() == "remote")
        .unwrap_or_else(|| panic!("the remote runtime's server is listed: {servers:?}"));
    assert_eq!(hosted.name, "fixture");
    assert_eq!(
        hosted.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert_eq!(
        hosted.state,
        cookie_agent_protocol::McpServerState::Connected
    );
    assert_eq!(hosted.tool_count, 2);

    // Its tools reach the model under their `<server>_<tool>` names, and a
    // call runs on the runtime's machine.
    fixture.model.enqueue(tool_call(
        "fixture_echo_text",
        &serde_json::json!({"text": "echoed by the remote MCP server"}),
    ));
    fixture.model.enqueue(final_answer("done"));
    let ended = fixture.run(session, "use the MCP tool").await;
    assert!(
        matches!(
            ended,
            EventPayload::RunFinished {
                outcome: cookie_agent_protocol::Open::Known(
                    cookie_agent_protocol::RunOutcome::Completed { .. }
                )
            }
        ),
        "{ended:?}"
    );
    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    let tools = offered_tools(&requests[0]);
    assert!(
        tools.iter().any(|tool| tool == "fixture_echo_text"),
        "{tools:?}"
    );
    assert!(
        requests[1].contains("echoed by the remote MCP server"),
        "{}",
        requests[1]
    );

    // A reconnect is routed to the runtime that hosts the server, which
    // reports the new connection.
    let mut changes = fixture.engine.subscribe_mcp_changes();
    changes.mark_unchanged();
    fixture
        .engine
        .reconnect_runtime_mcp_server(
            "remote",
            Some(canonical.to_string_lossy().as_ref()),
            "fixture".into(),
        )
        .await
        .expect("reconnect");
    tokio::time::timeout(WAIT, async {
        loop {
            let state = fixture
                .engine
                .list_mcp_servers()
                .into_iter()
                .find(|server| server.runtime_id.as_str() == "remote")
                .map(|server| server.state);
            if state == Some(cookie_agent_protocol::McpServerState::Connected) {
                break;
            }
            changes.changed().await.expect("changes");
        }
    })
    .await
    .expect("the server is connected again");
    fixture.engine.shutdown().await;
}

/// The real binary dials in to the daemon its machine's configuration names,
/// with the token it holds, serves a session's workspace, stops cleanly on
/// `SIGTERM`, and shows as disconnected once it exits.
#[tokio::test]
async fn a_runtime_dials_in_and_serves_a_session() {
    let fixture = Fixture::new(30_000).await;
    let token = cookie_agent_server::generate_token().expect("token");
    let server = Arc::new(cookie_agent_server::Server::new(fixture.engine.clone()));
    let listening = server
        .clone()
        .serve(0, token.clone())
        .await
        .expect("serve the daemon");
    // The runtime's machine names the daemon and its token in its own
    // configuration, so `--daemon` needs no URL.
    let remote_config = fixture.remote_home.join(".cookie-agent");
    private_dir(&remote_config);
    fs::write(
        remote_config.join("config.toml"),
        format!(
            "[server]\nurl = \"ws://{}/ws\"\ntoken = \"{}\"\n",
            listening.address(),
            token.as_str()
        ),
    )
    .expect("remote config");
    let mut runtime = tokio::process::Command::new(env!("CARGO_BIN_EXE_cookie"))
        .args(["runtime", "serve", "--daemon", "--id", "labpc"])
        .env("HOME", &fixture.remote_home)
        .env_remove("COOKIE_DAEMON_TOKEN")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the dialing runtime");
    let labpc_connected = |engine: &Engine| {
        engine
            .runtime_templates()
            .into_iter()
            .find(|template| template.runtime_id == "labpc")
            .map(|template| (template.lifetime, template.listing.connected))
    };
    tokio::time::timeout(WAIT, async {
        while labpc_connected(&fixture.engine).is_none_or(|(_, connected)| !connected) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runtime dials in");
    assert_eq!(
        labpc_connected(&fixture.engine),
        Some((cookie_agent_protocol::RuntimeLifetime::DialIn, true))
    );

    let session = fixture
        .engine
        .create_session(Fixture::selection())
        .expect("session")
        .session_id;
    let attachment = fixture
        .engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some("~/repo".into()),
            EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("attach the dial-in runtime");
    let canonical = fixture.repo.canonicalize().unwrap();
    assert_eq!(attachment.name.as_str(), "labpc_repo");
    assert_eq!(
        attachment.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );

    let pid = i32::try_from(runtime.id().expect("runtime pid")).expect("pid fits");
    // SAFETY: plain signal delivery to a child process this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0, "signal {pid}");
    let status = tokio::time::timeout(WAIT, runtime.wait())
        .await
        .expect("the runtime stops")
        .expect("exit status");
    assert!(status.success(), "{status}");
    tokio::time::timeout(WAIT, async {
        while labpc_connected(&fixture.engine).is_some_and(|(_, connected)| connected) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runtime shows disconnected");
    server.shutdown();
    listening.wait().await;
    fixture.engine.shutdown().await;
}
