//! The client corpus against this daemon: its recorder, and the replay of
//! its requests. `cookie_agent_protocol::corpus` describes the corpora and
//! their rules.
//!
//! The recorder drives a TUI-like [`Client`] through a session against an
//! in-process daemon whose model is a scripted provider and whose local
//! coding runtime runs behind the runtime protocol, and records both
//! connections: the client's as `client/<version>/session.jsonl` and the
//! coding runtime's as `runtime/<version>/coding.jsonl`.
//!
//! The replay sends the recorded requests to a fresh daemon of the same
//! setup, in their recorded order. Before each request it waits for the
//! replies that preceded it in the recording and for as many stored events
//! of each type as the recording had received by then. A string a replay's
//! reply holds where the recorded reply held another (a session, run,
//! upload, or approval ID, or an operation fingerprint) replaces it in
//! every later request. Every reply must be what it was: a result where the
//! recording has a result, and the same error code where it has an error.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    Authentication, CLIENT_PROTOCOL_VERSION, CatalogRevision, Client, ClientDelivery,
    ClientIdentity, ClientKind, RUNTIME_PROTOCOL_VERSION, SessionId,
    corpus::{self, Recorded, Recording},
    in_process_pair,
};
#[cfg(unix)]
use cookie_agent_protocol::{
    MessageFrame, MessageStream,
    corpus::{CorpusFile, Substitutions},
};
use cookie_agent_runtime_api::{RuntimeConnection, RuntimeConnector};
use jiff::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::Server;

/// A liveness bound, not a latency assertion.
const WAIT: Duration = Duration::from_secs(30);

/// A 1x1 PNG.
const IMAGE: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

const MODEL: &str = "custom.test/group/model";

/// The recorder's agent. Its memory rules keep the snapshot the corpus was
/// recorded with, from when memory had a default by mode.
const AGENT: &str = "---\ndescription: Corpus agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: ask\n  memory: allow\n  memory_write: allow\n---\nAnswer briefly.\n";

/// The model's answers, as OpenAI-compatible event streams: a `bash` call,
/// the approval reviewer's decision to ask the user, and the final answer.
fn model_turns() -> Vec<String> {
    let chunk = |delta: Value, finish: Option<&str>| {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": delta, "finish_reason": finish}]})
        )
    };
    let arguments = json!({"command": "printf 'corpus\\n'"}).to_string();
    vec![
        [
            chunk(json!({"content": "Running it."}), None),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "corpus-call", "type": "function",
                    "function": {"name": "bash", "arguments": arguments}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]
        .concat(),
        [
            chunk(json!({"content": r#"{"decision":"ask"}"#}), None),
            chunk(json!({}), Some("stop")),
        ]
        .concat(),
        [
            chunk(json!({"content": "It printed corpus."}), None),
            chunk(json!({}), Some("stop")),
        ]
        .concat(),
    ]
}

/// Serves `turns` in order, one per request, on a fresh local port.
async fn scripted_model(turns: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("model listener");
    let address = listener.local_addr().expect("model address");
    let task = tokio::spawn(async move {
        for body in turns {
            let (mut socket, _) = listener.accept().await.expect("model accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            // The headers, then the body they announce.
            let body_start = loop {
                let read = socket.read(&mut buffer).await.expect("model read");
                assert!(read > 0, "the request ended early");
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|length| length.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < body_start + length {
                let read = socket.read(&mut buffer).await.expect("model read");
                assert!(read > 0, "the request body ended early");
                request.extend_from_slice(&buffer[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("model response");
        }
    });
    (format!("http://{address}/v1"), task)
}

/// Runs the coding runtime in this process, recording what crosses the
/// connection when it has a recording.
struct CodingRuntime {
    recording: Option<Recording>,
}

#[async_trait::async_trait]
impl RuntimeConnector for CodingRuntime {
    async fn connect(&self) -> Result<RuntimeConnection, String> {
        let (daemon, runtime) = in_process_pair(64);
        tokio::spawn(cookie_agent_coding_runtime::server::serve(
            Box::new(runtime),
            cookie_agent_coding_runtime::server::ServeOptions::default(),
        ));
        Ok(RuntimeConnection::new(match &self.recording {
            Some(recording) => Box::new(recording.tap(daemon, "daemon", "runtime")),
            None => Box::new(daemon),
        }))
    }
}

/// A daemon set up as the recorder's: a scripted model, an agent that asks
/// before `bash`, and a local coding runtime behind the runtime protocol.
struct Daemon {
    directory: TempDir,
    engine: Engine,
    server: Arc<Server>,
    model: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn open(runtime: Option<Recording>) -> Self {
        let directory = crate::tests::private_tempdir();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let project = directory.path().join(".cookie-agent");
        crate::tests::create_private_test_dir(&project);
        let (endpoint, model) = scripted_model(model_turns()).await;
        let config = format!(
            r#"
[providers."custom.test"]
source = "custom"
endpoint = "{endpoint}"
adaptor = "openai-compatible"
auth = {{ method = "no-auth-v1", values = {{}} }}

[providers."custom.test".models."group/model"]
display_name = "Model"

[providers."custom.test".models."group/model".capabilities]
input = ["text", "image"]
output = ["text"]
context_tokens = 16384
output_tokens = 1024
tool_calling = true
parallel_tool_calls = true
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = false
native_replay = "unsupported"
media = {{ image = {{ mime_types = ["image/png"], max_bytes = 1048576, max_count = 1 }} }}
"#
        );
        std::fs::write(project.join("config.toml"), config).expect("configuration");
        let agents = project.join("agents");
        crate::tests::create_private_test_dir(&agents);
        std::fs::write(agents.join("primary.md"), AGENT).expect("agent");
        let mut config =
            cookie_agent_config::load_from_user_root(Some(&project)).expect("configuration");
        config.runtime.session_title.generate_on_first_turn = false;
        let provider_store = directory.path().join("provider-store");
        crate::tests::create_private_test_dir(&provider_store);
        let now = Timestamp::now();
        let catalog = Arc::new(CatalogSnapshot {
            revision: CatalogRevision::new(format!("sha256:{}", "1".repeat(64)))
                .expect("catalog revision"),
            source: CatalogSource::Bootstrap,
            state: CatalogRuntimeState {
                availability: CatalogAvailability::Bootstrap,
                age: CatalogAgeState::Current,
                last_error: None,
            },
            validated_at: now,
            last_checked_at: now,
            etag: None,
            providers: Arc::new(BTreeMap::new()),
            canonical_models: Arc::new(BTreeMap::new()),
            quarantine: Arc::new(Vec::new()),
        });
        let manager = Arc::new(
            ModelManager::new(
                config.runtime.providers.clone(),
                catalog,
                ProviderStore::open(&provider_store).expect("provider store"),
            )
            .expect("model manager"),
        );
        let mut runtimes = cookie_agent_runtime_api::RuntimeOptions::new(&workspace);
        runtimes.local =
            cookie_agent_runtime_api::LocalRuntime::Protocol(Arc::new(CodingRuntime {
                recording: runtime,
            }));
        let engine = Engine::open_async(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: workspace,
            config,
            model_manager: manager,
            runtimes,
        })
        .await
        .expect("engine");
        let server = Arc::new(Server::new(engine.clone()));
        Self {
            directory,
            engine,
            server,
            model,
        }
    }

    /// A connection to the daemon, authenticated as an in-process one is.
    fn connect(&self) -> cookie_agent_protocol::InProcessStream {
        let (client, service) = in_process_pair(256);
        let server = Arc::clone(&self.server);
        tokio::spawn(async move {
            let _ = server
                .serve_stream(service, Authentication::Authenticated)
                .await;
        });
        client
    }

    async fn shutdown(self) {
        self.server.shutdown();
        self.engine.shutdown().await;
        self.model.abort();
    }
}

/// The payload of a delivered stored event.
fn delivered_payload(delivery: &ClientDelivery) -> Option<Value> {
    match delivery {
        ClientDelivery::Live { message, .. } => match message.as_ref() {
            cookie_agent_protocol::EventSubscriptionMessage::Event { event } => {
                serde_json::to_value(&event.payload).ok()
            }
            _ => None,
        },
        ClientDelivery::ReplayEvent { event, .. } => serde_json::to_value(&event.payload).ok(),
        _ => None,
    }
}

/// Waits for a stored event whose payload `matches`.
async fn wait_for_event(
    deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>,
    what: &str,
    matches: impl Fn(&Value) -> bool,
) {
    tokio::time::timeout(WAIT, async {
        loop {
            let delivery = deliveries.recv().await.expect("deliveries");
            if delivered_payload(&delivery).is_some_and(|payload| matches(&payload)) {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {what} arrived"));
}

fn params<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("params")
}

/// The session the recorder drives, as the TUI would.
async fn drive(client: &Client) {
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    client
        .handshake_as(ClientIdentity::generate(
            ClientKind::new("tui").expect("client kind"),
        ))
        .await
        .expect("handshake");
    let snapshot = client
        .runtime_snapshot()
        .await
        .expect("runtime.snapshot.get");
    let catalog = serde_json::to_value(&snapshot).expect("catalog");
    assert!(
        catalog.to_string().contains("image/png"),
        "the model accepts images: {catalog}"
    );
    client
        .list_sessions(params(json!({})))
        .await
        .expect("session.list");
    // Since 1.8.0 the session belongs to a project.
    let project = client
        .create_project(params(json!({"name": "corpus", "emoji": "🍪"})))
        .await
        .expect("project.create");
    client.list_projects().await.expect("projects.list");
    let selection = json!({
        "agent": "primary",
        "model": {"model": MODEL, "variant": null},
        "preset": null,
    });
    let created = client
        .create_session(params(
            json!({"selection": selection, "project": project.project.id}),
        ))
        .await
        .expect("session.create");
    let session: SessionId = created.session.session_id;
    client
        .subscribe_events(session, None)
        .await
        .expect("events.subscribe");

    let image = base64::engine::general_purpose::STANDARD
        .decode(IMAGE)
        .expect("image");
    let upload = client
        .begin_upload(params(json!({
            "session_id": session, "byte_length": image.len(), "filename": "pixel.png",
        })))
        .await
        .expect("artifact.upload.begin");
    client
        .upload_chunk(params(json!({
            "upload_id": upload.upload_id, "offset": 0, "data": IMAGE,
        })))
        .await
        .expect("artifact.upload.chunk");
    let sha256 = cookie_agent_protocol::Sha256Digest::of_bytes(&image)
        .as_str()
        .to_owned();
    let finished = client
        .finish_upload(params(
            json!({"upload_id": upload.upload_id, "sha256": sha256}),
        ))
        .await
        .expect("artifact.upload.finish");
    let run = client
        .start_run(params(json!({
            "session_id": session,
            "client_run_id": "corpus-run",
            "selection": selection,
            "content": [
                {"type": "text", "text": "Run printf, then tell me what the image shows."},
                {"type": "file", "artifact": finished.artifact},
            ],
        })))
        .await
        .expect("run.start");

    // The `bash` call waits for approval while two steers arrive.
    wait_for_event(&mut deliveries, "escalation", |payload| {
        payload["type"] == "approval_step" && payload["step"]["type"] == "escalated"
    })
    .await;
    client
        .submit_input(params(json!({
            "session_id": session,
            "content": [{"type": "text", "text": "Keep it short."}],
            "delivery": "steer",
            "client_run_id": "corpus-steer",
        })))
        .await
        .expect("session.submit_input");
    let steered = client
        .steer_run(params(json!({
            "run_id": run.run_id,
            "content": [{"type": "text", "text": "One sentence."}],
        })))
        .await
        .expect("run.steer");
    assert!(steered.accepted, "{steered:?}");
    let approvals = client
        .list_approvals(params(
            json!({"root_session_id": session, "status": "escalated"}),
        ))
        .await
        .expect("approval.list");
    let [approval] = &approvals.approvals[..] else {
        panic!("one pending approval: {approvals:?}");
    };
    let request = serde_json::to_value(&approval.request).expect("approval request");
    client
        .respond_approval(params(json!({
            "session_id": session,
            "approval_id": request["approval_id"],
            "request_revision": request["revision"],
            "operation_fingerprint": request["operation_fingerprint"],
            "client_response_id": "corpus-approval",
            "decision": "approve_once",
        })))
        .await
        .expect("approval.respond");
    wait_for_event(&mut deliveries, "run_finished", |payload| {
        payload["type"] == "run_finished"
    })
    .await;

    client
        .get_session(params(json!({"session_id": session})))
        .await
        .expect("session.get");
    client
        .list_sessions(params(json!({})))
        .await
        .expect("session.list");
    client
        .read_artifact(params(json!({
            "session_id": session, "sha256": finished.artifact.sha256,
            "offset": 0, "length": 4096,
        })))
        .await
        .expect("artifact.read");
    client
        .unsubscribe_events(session)
        .await
        .expect("events.unsubscribe");
}

/// What identifies the recording machine and its user in `recordings`,
/// with the placeholder each is written as: the daemon's machine ID, and
/// what the runtime reports of its machine and its user (runtime protocol
/// 1.1.0 and 1.3.0), including the machine name the daemon derives from the
/// host name.
fn identifying(recordings: &[&Recording]) -> Vec<(String, &'static str)> {
    let messages = recordings
        .iter()
        .flat_map(|recording| recording.messages())
        .collect::<Vec<_>>();
    let found = |pointer: &str| {
        messages
            .iter()
            .find_map(|message| message.message.pointer(pointer)?.as_str())
            .map(str::to_owned)
    };
    let host = found("/result/machine/host_name");
    let machine_name = host.as_deref().map(|host| {
        let label = host
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        label
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>()
    });
    [
        (found("/result/machine_id"), corpus::MACHINE_ID_PLACEHOLDER),
        (found("/result/machine/id"), corpus::MACHINE_ID_PLACEHOLDER),
        (host, CORPUS_HOST_NAME),
        (machine_name, CORPUS_HOST_NAME),
        (found("/result/home"), CORPUS_HOME),
    ]
    .into_iter()
    .filter_map(|(value, placeholder)| {
        Some((value.filter(|value| !value.is_empty())?, placeholder))
    })
    .collect()
}

/// The messages of `recording` with what [`identifying`] found replaced.
fn anonymized(recording: &Recording, replacements: &[(String, &str)]) -> Vec<Recorded> {
    recording
        .messages()
        .into_iter()
        .map(|mut message| {
            let mut text = serde_json::to_string(&message.message).expect("message");
            for (value, placeholder) in replacements {
                text = text.replace(value.as_str(), placeholder);
            }
            message.message = serde_json::from_str(&text).expect("message");
            message
        })
        .collect()
}

/// The host name a recorded runtime reports.
const CORPUS_HOST_NAME: &str = "corpus-host";
/// The home directory a recorded runtime reports.
const CORPUS_HOME: &str = "/home/corpus";

/// Records the client and runtime corpora of this build's versions into
/// the corpus root `COOKIE_RECORD_CORPUS` names (a relative one from the
/// workspace root). It refuses a version whose directory exists there.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "records the frozen corpora: COOKIE_RECORD_CORPUS=<corpus root>"]
async fn record_the_client_and_runtime_corpora() {
    let root = PathBuf::from(
        std::env::var_os(corpus::RECORD_VARIABLE)
            .unwrap_or_else(|| panic!("set {} to the corpus root", corpus::RECORD_VARIABLE)),
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(root);
    let client_version = CLIENT_PROTOCOL_VERSION.to_string();
    let runtime_version = RUNTIME_PROTOCOL_VERSION.to_string();
    for (surface, version) in [("client", &client_version), ("runtime", &runtime_version)] {
        let directory = root.join(surface).join(version);
        assert!(
            !directory.exists(),
            "{} exists: a recorded corpus is never rewritten",
            directory.display()
        );
    }
    let runtime = Recording::default();
    let daemon = Daemon::open(Some(runtime.clone())).await;
    let recording = Recording::default();
    let client = Client::connect_stream(recording.tap(daemon.connect(), "client", "daemon"));
    drive(&client).await;
    client.shutdown();
    let scratch = daemon.directory.path().to_owned();
    daemon.shutdown().await;
    let replacements = identifying(&[&recording, &runtime]);
    for (surface, version, file, messages) in [
        (
            "client",
            &client_version,
            "session.jsonl",
            anonymized(&recording, &replacements),
        ),
        (
            "runtime",
            &runtime_version,
            "coding.jsonl",
            anonymized(&runtime, &replacements),
        ),
    ] {
        let path = Recording::write_new(&root, surface, version, file, &messages, &scratch)
            .expect("write the corpus");
        eprintln!("recorded {} messages in {}", messages.len(), path.display());
    }
}

/// What a replay has received so far.
#[derive(Default)]
#[cfg(unix)]
struct Received {
    replies: BTreeMap<String, Value>,
    /// Stored events by type, each counted once by session and sequence.
    events: BTreeMap<String, std::collections::BTreeSet<(String, u64)>>,
}

// Only the unix-only client corpus replay below uses this.
#[cfg(unix)]
impl Received {
    fn note_events(&mut self, events: &[Value]) {
        for event in events {
            if let (Some(kind), Some(seq)) =
                (event["payload"]["type"].as_str(), event["seq"].as_u64())
            {
                self.events
                    .entry(kind.to_owned())
                    .or_default()
                    .insert((event["session_id"].to_string(), seq));
            }
        }
    }

    /// Notes a message from the daemon; `methods` names each request ID's
    /// method.
    fn note(&mut self, message: &Value, methods: &BTreeMap<String, String>) {
        if let Some(id) = message.get("id") {
            if methods.get(&id.to_string()).map(String::as_str) == Some("events.subscribe")
                && let Some(events) = message["result"]["events"].as_array()
            {
                self.note_events(events);
            }
            self.replies.insert(id.to_string(), message.clone());
        } else if message["method"] == "events.subscription" && message["params"]["type"] == "event"
        {
            self.note_events(std::slice::from_ref(&message["params"]["event"]));
        }
    }

    /// The number of stored events of each type.
    fn counts(&self) -> Vec<(&str, usize)> {
        self.events
            .iter()
            .map(|(kind, events)| (kind.as_str(), events.len()))
            .collect()
    }

    fn event_count(&self, kind: &str) -> usize {
        self.events
            .get(kind)
            .map_or(0, std::collections::BTreeSet::len)
    }
}

/// A replay of one client corpus file against a fresh daemon.
// Only the unix-only client corpus replay below uses this.
#[cfg(unix)]
struct Replay<'a> {
    file: &'a CorpusFile,
    stream: cookie_agent_protocol::InProcessStream,
    /// The method of each request the client sent, by ID.
    methods: BTreeMap<String, String>,
    received: Received,
    substitutions: Substitutions,
    /// The requests whose replies were compared.
    checked: std::collections::BTreeSet<String>,
}

// Only the unix-only client corpus replay below uses this.
#[cfg(unix)]
impl Replay<'_> {
    /// Receives until the replay has what the recording had received by
    /// then (`expected`), and compares the replies that arrived.
    async fn catch_up(&mut self, expected: &Received) {
        let caught_up = |received: &Received| {
            expected
                .replies
                .keys()
                .all(|id| received.replies.contains_key(id))
                && expected
                    .events
                    .iter()
                    .all(|(kind, events)| received.event_count(kind) >= events.len())
        };
        while !caught_up(&self.received) {
            let frame = tokio::time::timeout(WAIT, self.stream.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "{}: the replay never received what the recording had by then: \
                         replies {:?} of {:?}, events {:?} of {:?}",
                        self.file.name,
                        self.received.replies.keys().collect::<Vec<_>>(),
                        expected.replies.keys().collect::<Vec<_>>(),
                        self.received.counts(),
                        expected.counts(),
                    )
                })
                .expect("the daemon's connection")
                .expect("the daemon is connected");
            let message: Value =
                serde_json::from_str(&frame.into_text().expect("text")).expect("JSON");
            self.received.note(&message, &self.methods);
        }
        for (id, recorded) in &expected.replies {
            if !self.checked.insert(id.clone()) {
                continue;
            }
            let replayed = &self.received.replies[id];
            let method = self.methods.get(id).map_or("?", String::as_str);
            match (recorded.get("result"), replayed.get("result")) {
                (Some(recorded), Some(replayed)) => self.substitutions.learn(recorded, replayed),
                (Some(_), None) => panic!(
                    "{}: `{method}` (id {id}) was accepted when recorded and is refused now: {}",
                    self.file.name, replayed["error"]
                ),
                (None, _) => assert_eq!(
                    (
                        &replayed["error"]["code"],
                        &replayed["error"]["data"]["code"]
                    ),
                    (
                        &recorded["error"]["code"],
                        &recorded["error"]["data"]["code"]
                    ),
                    "{}: `{method}` (id {id}) failed differently",
                    self.file.name
                ),
            }
        }
    }
}

/// Replays the client's requests of `file` against a fresh daemon.
#[cfg(unix)]
async fn replay(file: &CorpusFile) {
    let daemon = Daemon::open(None).await;
    let methods = file
        .messages
        .iter()
        .filter(|message| message.from == "client" && message.is_request())
        .map(|message| {
            (
                message.message["id"].to_string(),
                message.method().unwrap_or_default().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut substitutions = Substitutions::default();
    substitutions.insert(
        corpus::SCRATCH_PLACEHOLDER,
        daemon.directory.path().to_string_lossy(),
    );
    let mut replay = Replay {
        file,
        stream: daemon.connect(),
        methods,
        received: Received::default(),
        substitutions,
        checked: std::collections::BTreeSet::new(),
    };
    // What the recording had received before the message at hand.
    let mut expected = Received::default();
    for message in &file.messages {
        if message.from == "client" {
            replay.catch_up(&expected).await;
            let request = replay.substitutions.apply(&message.message);
            replay
                .stream
                .send(MessageFrame::Value(request))
                .await
                .expect("the daemon's connection");
        } else {
            expected.note(&message.message, &replay.methods);
        }
    }
    replay.catch_up(&expected).await;
    assert_eq!(
        replay.checked.len(),
        replay.methods.len(),
        "{}: every request was answered",
        file.name
    );
    drop(replay);
    daemon.shutdown().await;
}

/// Every frozen client corpus replays against this daemon: each request it
/// recorded is accepted as it was.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn recorded_client_requests_are_accepted_by_this_daemon() {
    let versions = corpus::frozen_versions("client");
    assert!(!versions.is_empty(), "no frozen client corpus");
    for version in versions {
        for file in corpus::frozen("client", version) {
            replay(&file).await;
        }
    }
}
