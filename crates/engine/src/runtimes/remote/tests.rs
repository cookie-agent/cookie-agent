//! The runtime protocol end to end: the daemon's client against the coding
//! runtime's server over an in-memory frame pair.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_coding_runtime::server::{ServeOptions, serve};
use cookie_agent_protocol::{
    ArtifactReference, MessageFrame, PersistedToolResult, RunId, SessionId, ToolCallId,
    ToolOutputChunk, Transport, TransportError, in_process_pair,
};
use cookie_agent_runtime_api::{
    BackgroundJob, BackgroundJobs, BackgroundLauncher, BackgroundReport, BackgroundTask,
    CodingToolsFactory, PreparedExecutor, PreparedTool, ProgressCapture, ProgressSink,
    RuntimeConnector, SessionToolContext, StdinWrite, ToolArtifacts, ToolCall, ToolCompletion,
    ToolError, ToolExecutionContext, ToolOrigin, ToolPreparationContext, ToolProgress,
    ToolProvider, ToolSpec, ToolStdin, TurnAgentContext, authoring,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ProtocolTemplate, RuntimeClient, runtime_settings, started};
use crate::runtime::tests::support::{custom_fixture, private_tempdir};

const WAIT: Duration = Duration::from_secs(20);

fn turn_context() -> Arc<TurnAgentContext> {
    Arc::new(TurnAgentContext {
        agent: cookie_agent_protocol::AgentId::new("test").expect("agent ID"),
        model: "test/model".parse().expect("model key"),
        adapter: cookie_agent_protocol::AdaptorId::OpenaiChat,
        adapter_family: cookie_agent_runtime_api::AdapterFamily::OpenaiChat,
        capabilities: cookie_agent_protocol::ModelCapabilities {
            input: std::collections::BTreeSet::from([cookie_agent_protocol::Modality::Text]),
            output: std::collections::BTreeSet::from([cookie_agent_protocol::Modality::Text]),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: cookie_agent_protocol::ReplayCapability::Optional,
            cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
            media: std::collections::BTreeMap::new(),
        },
    })
}

/// A transport that can be cut, as when a runtime process dies.
struct Breakable {
    inner: cookie_agent_protocol::InProcessStream,
    broken: CancellationToken,
}

#[async_trait]
impl Transport for Breakable {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        if self.broken.is_cancelled() {
            return Err(TransportError::Closed);
        }
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        tokio::select! {
            () = self.broken.cancelled() => Ok(None),
            frame = self.inner.recv() => frame,
        }
    }
}

/// Serves a coding runtime per connection; cancelling `broken` cuts every
/// connection.
struct TestConnector {
    tools: CodingToolsFactory,
    broken: Mutex<CancellationToken>,
}

impl TestConnector {
    fn new(tools: CodingToolsFactory) -> Arc<Self> {
        Arc::new(Self {
            tools,
            broken: Mutex::new(CancellationToken::new()),
        })
    }

    fn builtin() -> Arc<Self> {
        Self::new(ServeOptions::default().tools)
    }

    /// Cuts the current connections; later connections work.
    fn cut(&self) {
        let mut broken = self.broken.lock().unwrap();
        broken.cancel();
        *broken = CancellationToken::new();
    }
}

#[async_trait]
impl RuntimeConnector for TestConnector {
    async fn connect(&self) -> Result<Box<dyn Transport>, String> {
        let (daemon, runtime) = in_process_pair(64);
        tokio::spawn(serve(
            Box::new(runtime),
            ServeOptions {
                workspace: None,
                tools: Arc::clone(&self.tools),
            },
        ));
        Ok(Box::new(Breakable {
            inner: daemon,
            broken: self.broken.lock().unwrap().clone(),
        }))
    }
}

/// A started protocol runtime for `workspace` and its one provider.
async fn connect(
    connector: &Arc<TestConnector>,
    workspace: &std::path::Path,
) -> (crate::StartedRuntime, Arc<dyn ToolProvider>) {
    let transport = connector.connect().await.unwrap();
    let settings = runtime_settings(&custom_fixture().0.config);
    let (client, result) =
        RuntimeClient::initialize(transport, "remote", Some(workspace), &settings)
            .await
            .unwrap();
    let started = started(client, result).unwrap();
    let provider = Arc::clone(&started.providers[0]);
    (started, provider)
}

async fn prepare(
    provider: &Arc<dyn ToolProvider>,
    started: &crate::StartedRuntime,
    name: &str,
    arguments: serde_json::Value,
) -> PreparedTool {
    provider
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                origin: ToolOrigin::Runtime(Arc::new(started_info(started))),
                turn_context: turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: name.into(),
                arguments,
            },
        )
        .await
        .expect("prepared")
}

fn started_info(started: &crate::StartedRuntime) -> cookie_agent_runtime_api::RuntimeInfo {
    cookie_agent_runtime_api::RuntimeInfo::new(
        started.info.id(),
        started.info.kind(),
        started.info.attachment_name(),
        started.info.workspace().cloned(),
    )
}

/// Records captured output and passes display deltas on.
#[derive(Debug, Default)]
struct RecordingCapture {
    chunks: Mutex<Vec<ToolOutputChunk>>,
}

impl RecordingCapture {
    fn stream(&self, name: &str) -> String {
        self.chunks
            .lock()
            .unwrap()
            .iter()
            .filter(|chunk| chunk.stream.as_deref() == Some(name))
            .map(|chunk| chunk.text.as_str())
            .collect()
    }
}

#[async_trait]
impl ProgressCapture for RecordingCapture {
    async fn emit(
        &self,
        progress: ToolProgress,
        display: Option<mpsc::OwnedPermit<ToolProgress>>,
    ) -> Result<(), ToolError> {
        self.chunks
            .lock()
            .unwrap()
            .extend(progress.output.iter().cloned());
        if let Some(display) = display {
            display.send(progress);
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct RecordingArtifacts {
    retained: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl ToolArtifacts for RecordingArtifacts {
    fn retain(
        &self,
        _session: SessionId,
        content: &[u8],
    ) -> Result<(ArtifactReference, String), ToolError> {
        self.retained.lock().unwrap().push(content.to_vec());
        let digest = cookie_agent_protocol::Sha256Digest::of_bytes(content)
            .as_str()
            .to_owned();
        Ok((
            cookie_agent_runtime_api::protocol::artifact_reference(&digest),
            digest,
        ))
    }

    async fn read_page(
        self: Arc<Self>,
        _session: SessionId,
        _target: cookie_agent_protocol::ArtifactReadTarget,
        _offset: u64,
        _limit: u64,
    ) -> Result<cookie_agent_runtime_api::ArtifactReadPage, ToolError> {
        Err(ToolError::execution("unused"))
    }
}

/// Background jobs whose reports arrive on a channel, as the engine would
/// commit them.
#[derive(Debug)]
struct RecordingJobs {
    capture: Arc<RecordingCapture>,
    reports: mpsc::UnboundedSender<BackgroundReport>,
    display: Mutex<Vec<mpsc::Receiver<ToolProgress>>>,
}

#[async_trait]
impl BackgroundJobs for RecordingJobs {
    async fn reserve(&self, _session: SessionId) -> Result<BackgroundJob, ToolError> {
        let (sender, receiver) = mpsc::channel(64);
        self.display.lock().unwrap().push(receiver);
        Ok(BackgroundJob::new(
            ProgressSink::with_capture(sender, self.capture.clone()),
            Box::new(RecordingLauncher {
                reports: self.reports.clone(),
            }),
        ))
    }
}

struct RecordingLauncher {
    reports: mpsc::UnboundedSender<BackgroundReport>,
}

impl BackgroundLauncher for RecordingLauncher {
    fn launch(self: Box<Self>, task: BackgroundTask) -> Result<(), ToolError> {
        tokio::spawn(async move {
            let _ = self.reports.send(task.await);
        });
        Ok(())
    }
}

struct Call {
    context: ToolExecutionContext,
    capture: Arc<RecordingCapture>,
    artifacts: Arc<RecordingArtifacts>,
    _display: mpsc::Receiver<ToolProgress>,
}

fn call_context(
    cancellation: CancellationToken,
    stdin: Option<ToolStdin>,
    background: Option<Arc<dyn BackgroundJobs>>,
) -> Call {
    let capture = Arc::new(RecordingCapture::default());
    let artifacts = Arc::new(RecordingArtifacts::default());
    let (sender, display) = mpsc::channel(256);
    Call {
        context: ToolExecutionContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            progress: ProgressSink::with_capture(sender, capture.clone()),
            cancellation,
            stdin,
            turn_context: turn_context(),
            artifacts: artifacts.clone(),
            background,
        },
        capture,
        artifacts,
        _display: display,
    }
}

async fn execute(
    prepared: PreparedTool,
    context: ToolExecutionContext,
) -> Result<ToolCompletion, ToolError> {
    let executor = prepared.executor().lock().await.take().expect("executor");
    executor.revalidate().await?;
    tokio::time::timeout(WAIT, executor.execute(context))
        .await
        .expect("execution finishes in time")
}

#[tokio::test]
async fn streamed_output_and_stdin_cross_the_protocol() {
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let (started, provider) = connect(&connector, workspace.path()).await;
    let names = provider
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .unwrap()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "write", "edit", "bash", "webfetch"]);
    let prepared = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({
            "command": "read line; echo \"got:$line\"; echo diagnostics >&2",
            "interactive": true,
        }),
    )
    .await;
    let declaration = prepared.declaration().expect("reported metadata").clone();
    assert_eq!(declaration.permission_name, "bash");
    assert_eq!(
        declaration.permission_resource.as_deref(),
        Some("read line; echo \"got:$line\"; echo diagnostics >&2")
    );
    assert_eq!(declaration.presentation.title.as_str(), "bash");
    assert_eq!(
        prepared.policy_labels(),
        std::slice::from_ref(&declaration.permission_resource)
    );
    // The sync metadata methods defer to preparation.
    assert!(
        provider
            .get_permission_resource("bash", &serde_json::json!({"command": "true"}))
            .is_err()
    );

    let (stdin, input) = ToolStdin::channel(4);
    let call = call_context(CancellationToken::new(), Some(input), None);
    stdin
        .send(StdinWrite {
            data: b"hello\n".to_vec(),
            eof: true,
        })
        .await
        .unwrap();
    let completion = execute(prepared, call.context).await.unwrap();
    assert!(!completion.failed);
    assert_eq!(call.capture.stream("stdout"), "got:hello\n");
    assert_eq!(call.capture.stream("stderr"), "diagnostics\n");
    assert!(call.artifacts.retained.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_reaches_the_call_in_the_runtime() {
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let (started, provider) = connect(&connector, workspace.path()).await;
    let prepared = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({"command": "echo started; sleep 30"}),
    )
    .await;
    let cancellation = CancellationToken::new();
    let call = call_context(cancellation.clone(), None, None);
    let capture = Arc::clone(&call.capture);
    let started_at = std::time::Instant::now();
    let execution = tokio::spawn(execute(prepared, call.context));
    while capture.stream("stdout").is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(started_at.elapsed() < WAIT, "the command never started");
    }
    cancellation.cancel();
    let result = tokio::time::timeout(WAIT, execution)
        .await
        .expect("the call stops")
        .unwrap();
    assert!(started_at.elapsed() < Duration::from_secs(15));
    match result {
        Ok(completion) => assert!(completion.failed || completion.result.display.is_some()),
        Err(error) => assert!(
            !matches!(error, ToolError::RuntimeUnavailable(_)),
            "{error}"
        ),
    }
}

/// A tool that returns one retained attachment.
struct MediaTool;

const PIXEL: &[u8] = b"not really an image, but bytes all the same";

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[async_trait]
impl ToolProvider for MediaTool {
    fn provider_id(&self) -> &'static str {
        "test.media"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "snapshot".into(),
            collision: Default::default(),
            permission_name: "read".into(),
            description: "Attach a snapshot.".into(),
            parameters: authoring::schema::<NoArgs>(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            output: Default::default(),
        }])
    }

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError> {
        Ok("read")
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Ok(("read", Some("snapshot".into())))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("snapshot".into())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let resource = authoring::prepared_resource(
            cookie_agent_protocol::PermissionAction::Read,
            "snapshot",
            b"snapshot",
            b"snapshot",
            cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
            cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = authoring::prepared_operation(
            "snapshot",
            &serde_json::json!({}),
            vec![(cookie_agent_protocol::PermissionAction::Read, "snapshot")],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(
            operation,
            serde_json::json!({}),
            Some(cookie_agent_runtime_api::PreparedSerializationKey::new(
                b"snapshot".to_vec(),
            )),
            Box::new(MediaExecutor),
        )
    }
}

struct MediaExecutor;

#[async_trait]
impl PreparedExecutor for MediaExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let attachment =
            context.retain_validated_attachment("image/png", Some("dot.png".into()), PIXEL)?;
        Ok(ToolCompletion::single(PersistedToolResult {
            title: cookie_agent_protocol::SafeDisplayText::new("Snapshot").unwrap(),
            output: "Attached a snapshot.".into(),
            display: None,
            retained_output: None,
            metadata: serde_json::Value::Null,
            truncation: None,
            attachments: vec![attachment],
            additional_messages: Vec::new(),
        }))
    }
}

#[tokio::test]
async fn retained_media_travels_inline_and_lands_in_the_daemon_store() {
    let workspace = private_tempdir();
    let connector = TestConnector::new(Arc::new(|_| vec![Arc::new(MediaTool)]));
    let (started, provider) = connect(&connector, workspace.path()).await;
    let prepared = prepare(&provider, &started, "snapshot", serde_json::json!({})).await;
    assert_eq!(
        prepared.serialization_key(),
        Some(&cookie_agent_runtime_api::PreparedSerializationKey::new(
            b"snapshot".to_vec()
        ))
    );
    let call = call_context(CancellationToken::new(), None, None);
    let completion = execute(prepared, call.context).await.unwrap();
    assert_eq!(call.artifacts.retained.lock().unwrap().as_slice(), [PIXEL]);
    let attachment = &completion.result.attachments[0];
    assert_eq!(
        attachment.reference,
        cookie_agent_runtime_api::protocol::artifact_reference(
            cookie_agent_protocol::Sha256Digest::of_bytes(PIXEL).as_str()
        )
    );
    assert_eq!(attachment.byte_length, PIXEL.len() as u64);
}

#[tokio::test]
async fn a_background_bash_job_reports_through_the_daemon() {
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let (started, provider) = connect(&connector, workspace.path()).await;
    let prepared = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({
            "command": "sleep 0.2; echo background-out; echo background-err >&2",
            "background": true,
        }),
    )
    .await;
    let (reports, mut reported) = mpsc::unbounded_channel();
    let job_capture = Arc::new(RecordingCapture::default());
    let jobs = Arc::new(RecordingJobs {
        capture: Arc::clone(&job_capture),
        reports,
        display: Mutex::default(),
    });
    let call = call_context(CancellationToken::new(), None, Some(jobs));
    let completion = execute(prepared, call.context).await.unwrap();
    assert_eq!(completion.result.metadata["background"], true);
    let report = tokio::time::timeout(WAIT, reported.recv())
        .await
        .expect("the job reports")
        .expect("report");
    assert_eq!(report.element, "background_bash");
    assert!(
        report.body.contains("exited with code 0"),
        "{}",
        report.body
    );
    assert_eq!(job_capture.stream("stdout"), "background-out\n");
    assert_eq!(job_capture.stream("stderr"), "background-err\n");
}

#[tokio::test]
async fn transport_loss_fails_calls_and_jobs_in_flight() {
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let (started, provider) = connect(&connector, workspace.path()).await;
    let job = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({"command": "sleep 30", "background": true}),
    )
    .await;
    let (reports, mut reported) = mpsc::unbounded_channel();
    let jobs = Arc::new(RecordingJobs {
        capture: Arc::new(RecordingCapture::default()),
        reports,
        display: Mutex::default(),
    });
    let call = call_context(CancellationToken::new(), None, Some(jobs));
    execute(job, call.context).await.unwrap();

    let running = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({"command": "echo started; sleep 30"}),
    )
    .await;
    let call = call_context(CancellationToken::new(), None, None);
    let capture = Arc::clone(&call.capture);
    let execution = tokio::spawn(execute(running, call.context));
    while capture.stream("stdout").is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let connection = started.connection.clone().expect("protocol connection");
    connector.cut();
    let error = tokio::time::timeout(WAIT, execution)
        .await
        .expect("the call fails at once")
        .unwrap()
        .expect_err("the call cannot finish");
    assert!(
        matches!(error, ToolError::RuntimeUnavailable(_)),
        "{error:?}"
    );
    let report = tokio::time::timeout(WAIT, reported.recv())
        .await
        .expect("the job completes")
        .expect("report");
    assert_eq!(report.element, "background_job");
    assert!(report.summary.contains("exited"), "{}", report.summary);
    assert!(connection.is_closed());

    // New calls fail fast with the same code.
    let error = provider
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                origin: ToolOrigin::Base,
                turn_context: turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "true"}),
            },
        )
        .await
        .err()
        .expect("unavailable");
    assert!(
        matches!(error, ToolError::RuntimeUnavailable(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_lost_instance_fails_and_restarts_under_a_new_generation() {
    let (fixture, _) = custom_fixture();
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let registry = &fixture.engine.inner.runtimes;
    registry
        .register_template(Arc::new(ProtocolTemplate {
            runtime_id: "remote".into(),
            takes_workspace: true,
            fixed_workspace: None,
            connector: connector.clone(),
            settings: runtime_settings(&fixture.config),
        }))
        .unwrap();
    let mut failures = registry.subscribe_failures();
    let first = registry
        .ensure("remote", Some(workspace.path()))
        .await
        .unwrap();
    assert_eq!(
        first.info().kind(),
        cookie_agent_protocol::RuntimeKind::Coding
    );
    assert!(!first.providers().is_empty());

    connector.cut();
    let failure = tokio::time::timeout(WAIT, failures.recv())
        .await
        .expect("the loss is reported")
        .unwrap();
    assert_eq!(failure.runtime_id, "remote");
    assert!(failure.reason.contains("exited"), "{}", failure.reason);
    let status = registry
        .instances()
        .into_iter()
        .find(|instance| instance.runtime_id == "remote")
        .unwrap()
        .status;
    assert_eq!(status, crate::RuntimeInstanceStatus::Failed);
    assert!(first.providers().is_empty());

    let second = registry
        .ensure("remote", Some(workspace.path()))
        .await
        .unwrap();
    assert_ne!(second.generation(), first.generation());
    assert_eq!(
        second.info().attachment_name(),
        first.info().attachment_name()
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_local_runtime_runs_behind_the_protocol() {
    let (fixture, selection) = custom_fixture();
    fixture.engine.shutdown().await;
    let directory = fixture._directory.path();
    std::fs::write(directory.join("AGENTS.md"), "local workspace context").unwrap();
    let skill = directory.join(".agents/skills/release-notes");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: release-notes\ndescription: Write release notes\n---\nCollect the changes.\n",
    )
    .unwrap();
    let engine = crate::Engine::open_async(crate::EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions {
            local: cookie_agent_runtime_api::LocalRuntime::Protocol(
                cookie_agent_coding_runtime::server::in_process_connector(),
            ),
            ..cookie_agent_runtime_api::RuntimeOptions::new(directory)
        },
    })
    .await
    .expect("engine with a protocol local runtime");
    assert!(engine.inner.runtimes.local().connection().is_some());
    let session = engine.create_session(selection).unwrap().session_id;
    let composition = engine.compose_session_tools(session, |_, _| true).unwrap();
    for name in ["read", "write", "edit", "bash", "webfetch"] {
        let tool = composition.get(name).expect(name);
        assert_eq!(
            tool.source.origin.runtime().map(|runtime| runtime.id()),
            Some("local"),
            "{name}"
        );
    }
    let agent_md = engine.load_agent_md(session, None, None).await.unwrap();
    assert_eq!(agent_md.len(), 1);
    assert_eq!(agent_md[0].entries[0].content, "local workspace context");
    assert!(
        engine
            .inner
            .runtimes
            .local()
            .skills()
            .get("release-notes")
            .is_some()
    );
    let connection = engine.inner.runtimes.local().connection().unwrap();
    engine.shutdown().await;
    tokio::time::timeout(WAIT, connection.closed())
        .await
        .expect("shutdown closes the local runtime");
}
