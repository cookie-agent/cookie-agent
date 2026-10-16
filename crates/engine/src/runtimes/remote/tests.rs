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
    /// Connections made so far.
    connects: std::sync::atomic::AtomicUsize,
    /// While set, connecting fails, as when a host is unreachable.
    refuse: std::sync::atomic::AtomicBool,
}

impl TestConnector {
    fn new(tools: CodingToolsFactory) -> Arc<Self> {
        Arc::new(Self {
            tools,
            broken: Mutex::new(CancellationToken::new()),
            connects: std::sync::atomic::AtomicUsize::new(0),
            refuse: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn builtin() -> Arc<Self> {
        Self::new(ServeOptions::default().tools)
    }

    fn set_refuse(&self, refuse: bool) {
        self.refuse
            .store(refuse, std::sync::atomic::Ordering::SeqCst);
    }

    fn connects(&self) -> usize {
        self.connects.load(std::sync::atomic::Ordering::SeqCst)
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
    async fn connect(
        &self,
        _workspace: Option<&std::path::Path>,
    ) -> Result<cookie_agent_runtime_api::RuntimeConnection, String> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("connection refused".into());
        }
        self.connects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (daemon, runtime) = in_process_pair(64);
        tokio::spawn(serve(
            Box::new(runtime),
            ServeOptions {
                workspace: None,
                tools: Arc::clone(&self.tools),
                mcp_reconnect: None,
            },
        ));
        Ok(cookie_agent_runtime_api::RuntimeConnection::new(Box::new(
            Breakable {
                inner: daemon,
                broken: self.broken.lock().unwrap().clone(),
            },
        )))
    }
}

/// A started protocol runtime for `workspace` and its one provider.
async fn connect(
    connector: &Arc<TestConnector>,
    workspace: &std::path::Path,
) -> (crate::StartedRuntime, Arc<dyn ToolProvider>) {
    let connection = connector.connect(Some(workspace)).await.unwrap();
    let settings = runtime_settings(&custom_fixture().0.config, "remote");
    let (client, result) = RuntimeClient::initialize(
        connection,
        "remote",
        Some(workspace),
        &settings,
        Duration::from_secs(1),
        None,
    )
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
        started.info.kind().clone(),
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
            cookie_agent_protocol::PermissionAction::READ,
            "snapshot",
            b"snapshot",
            b"snapshot",
            cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
            cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = authoring::prepared_operation(
            "snapshot",
            &serde_json::json!({}),
            vec![(cookie_agent_protocol::PermissionAction::READ, "snapshot")],
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

fn instance_status(
    registry: &crate::runtimes::RuntimeRegistry,
    runtime_id: &str,
) -> crate::RuntimeInstanceStatus {
    registry
        .instances()
        .into_iter()
        .find(|instance| instance.runtime_id == runtime_id)
        .expect("instance listed")
        .status
}

async fn wait_for_status(
    registry: &crate::runtimes::RuntimeRegistry,
    runtime_id: &str,
    status: crate::RuntimeInstanceStatus,
) {
    tokio::time::timeout(WAIT, async {
        while instance_status(registry, runtime_id) != status {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("`{runtime_id}` never became {status:?}"));
}

#[tokio::test]
async fn a_dropped_instance_reconnects_with_its_tools_published_meanwhile() {
    let (fixture, _) = custom_fixture();
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let registry = &fixture.engine.inner.runtimes;
    // A first attempt late enough to watch the instance reconnect.
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(400),
    ]));
    let mut failures = registry.subscribe_failures();
    let instance = registry
        .ensure("remote", Some(workspace.path()))
        .await
        .unwrap();
    let first_generation = instance.generation();
    let first_connection = instance.connection().unwrap();
    let attachment = instance
        .attachment(cookie_agent_protocol::AttachmentName::new("remote_ws").unwrap())
        .unwrap();

    connector.cut();
    wait_for_status(
        registry,
        "remote",
        crate::RuntimeInstanceStatus::Reconnecting,
    )
    .await;
    // The tools stay published; calls fail fast with `runtime_unavailable`.
    let reconnecting = registry.instance(&attachment).expect("still published");
    assert!(Arc::ptr_eq(&reconnecting, &instance));
    let (generation, providers) = instance.sources();
    assert_eq!(generation, first_generation);
    let error = providers[0]
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                origin: instance.origin(),
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

    wait_for_status(registry, "remote", crate::RuntimeInstanceStatus::Ready).await;
    // The same instance under a new generation and connection.
    let restored = registry.instance(&attachment).expect("restored");
    assert!(Arc::ptr_eq(&restored, &instance));
    assert!(instance.generation() > first_generation);
    assert!(first_connection.is_closed());
    assert!(!instance.connection().unwrap().is_closed());
    assert_eq!(connector.connects(), 2);
    assert!(failures.try_recv().is_err(), "a reconnect is no failure");

    // It reconnects again after a second drop.
    connector.cut();
    wait_for_status(
        registry,
        "remote",
        crate::RuntimeInstanceStatus::Reconnecting,
    )
    .await;
    wait_for_status(registry, "remote", crate::RuntimeInstanceStatus::Ready).await;
    assert_eq!(connector.connects(), 3);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn an_instance_that_cannot_reconnect_fails_and_is_revived_on_demand() {
    let (fixture, _) = custom_fixture();
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let registry = &fixture.engine.inner.runtimes;
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(10);
        3
    ]));
    let mut failures = registry.subscribe_failures();
    let instance = registry
        .ensure("remote", Some(workspace.path()))
        .await
        .unwrap();
    let first_generation = instance.generation();
    let attachment = instance
        .attachment(cookie_agent_protocol::AttachmentName::new("remote_ws").unwrap())
        .unwrap();

    connector.set_refuse(true);
    connector.cut();
    let failure = tokio::time::timeout(WAIT, failures.recv())
        .await
        .expect("the failure is reported")
        .unwrap();
    assert_eq!(failure.runtime_id, "remote");
    assert_eq!(
        failure.workspace.as_deref(),
        Some(
            workspace
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(failure.reason.contains("exited"), "{}", failure.reason);
    assert!(
        failure
            .reason
            .contains("reconnecting failed 3 times, last: connection refused"),
        "{}",
        failure.reason
    );
    assert_eq!(
        instance_status(registry, "remote"),
        crate::RuntimeInstanceStatus::Failed
    );
    // Its tools are dropped.
    assert!(registry.instance(&attachment).is_none());
    assert!(instance.providers().is_empty());

    // A retry fails while the runtime is unreachable, then revives it.
    let error = registry
        .ensure("remote", Some(workspace.path()))
        .await
        .err()
        .expect("still unreachable");
    assert!(error.to_string().contains("connection refused"), "{error}");
    connector.set_refuse(false);
    let revived = registry
        .ensure("remote", Some(workspace.path()))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&revived, &instance));
    assert!(instance.generation() > first_generation);
    assert!(!instance.providers().is_empty());
    assert_eq!(
        instance_status(registry, "remote"),
        crate::RuntimeInstanceStatus::Ready
    );
    assert!(registry.instance(&attachment).is_some());
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

fn register_remote(
    fixture: &crate::runtime::tests::support::Fixture,
    connector: &Arc<TestConnector>,
) {
    fixture
        .engine
        .inner
        .runtimes
        .register_template(Arc::new(ProtocolTemplate::new(
            "remote",
            true,
            connector.clone(),
            runtime_settings(&fixture.config, "remote"),
            WAIT,
            Duration::from_secs(1),
        )))
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn instances_are_keyed_by_the_workspace_the_runtime_reports() {
    let (fixture, selection) = custom_fixture();
    let root = private_tempdir();
    let api = root.path().join("api");
    std::fs::create_dir(&api).unwrap();
    let canonical = api.canonicalize().unwrap();
    let first_link = root.path().join("first");
    let second_link = root.path().join("second");
    std::os::unix::fs::symlink(&api, &first_link).unwrap();
    std::os::unix::fs::symlink(&api, &second_link).unwrap();
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let registry = &fixture.engine.inner.runtimes;

    // The daemon passes the path as given; the runtime resolves it.
    let instance = registry.ensure("remote", Some(&first_link)).await.unwrap();
    assert_eq!(
        instance.info().workspace().unwrap().root(),
        canonical.as_path()
    );
    assert_eq!(connector.connects(), 1);
    // The same spelling and the canonical path reuse it without connecting.
    for path in [&first_link, &canonical] {
        let again = registry.ensure("remote", Some(path)).await.unwrap();
        assert!(Arc::ptr_eq(&again, &instance));
    }
    assert_eq!(connector.connects(), 1);
    // A new spelling must start a runtime to learn where it resolves; that
    // one is stopped again and the running instance is shared.
    let deduplicated = registry.ensure("remote", Some(&second_link)).await.unwrap();
    assert!(Arc::ptr_eq(&deduplicated, &instance));
    assert_eq!(connector.connects(), 2);
    let remote = registry
        .instances()
        .into_iter()
        .filter(|instance| instance.runtime_id == "remote")
        .collect::<Vec<_>>();
    assert_eq!(remote.len(), 1, "{remote:?}");
    assert_eq!(
        remote[0].workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );

    // A session records the canonical path, which reopening asks for.
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    let attachment = fixture
        .engine
        .attach_runtime(
            session,
            "remote",
            Some(second_link.clone()),
            cookie_agent_protocol::EventOrigin::new("user").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        attachment.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    fixture.engine.shutdown().await;
}

/// `[runtimes.local.mcp]` with the stdio fixture server, which refuses to
/// start while `refuse` exists.
fn local_mcp_config(
    config: &cookie_agent_config::LoadedConfiguration,
    refuse: &std::path::Path,
) -> cookie_agent_config::LoadedConfiguration {
    let mut config = config.clone();
    let server = cookie_agent_config::McpServerConfig {
        command: Some(crate::runtime::tests::support::python_command().into()),
        args: vec![
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../mcp/tests/fixtures/mcp_server.py"
            )
            .into(),
        ],
        env: std::collections::BTreeMap::from([(
            "MCP_FIXTURE_REFUSE_FILE".to_owned(),
            refuse.to_string_lossy().into_owned(),
        )]),
        cwd: None,
        url: None,
        headers: std::collections::BTreeMap::new(),
        oauth: cookie_agent_config::McpOAuthConfig::default(),
        enabled: true,
        timeout_ms: Some(30_000),
    };
    config.runtimes.insert(
        cookie_agent_identity::RuntimeId::new("local").unwrap(),
        cookie_agent_config::RuntimeTemplateConfig {
            command: Vec::new(),
            env: std::collections::BTreeMap::new(),
            startup_timeout_ms: 10_000,
            shutdown_grace_ms: 3_000,
            mcp_servers: std::collections::BTreeMap::from([("fixture".to_owned(), server)]),
        },
    );
    config
}

async fn open_with_local_mcp(
    fixture: &crate::runtime::tests::support::Fixture,
    refuse: &std::path::Path,
) -> crate::Engine {
    let directory = fixture._directory.path();
    crate::Engine::open_async(crate::EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.to_owned(),
        config: local_mcp_config(&fixture.config, refuse),
        model_manager: Arc::clone(&fixture.manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions {
            local: cookie_agent_runtime_api::LocalRuntime::Protocol(
                cookie_agent_coding_runtime::server::in_process_connector_with_options(
                    ServeOptions {
                        mcp_reconnect: Some(
                            cookie_agent_runtime_api::reconnect::ReconnectPolicy::with_delays(
                                vec![Duration::from_millis(20)],
                            ),
                        ),
                        ..ServeOptions::default()
                    },
                ),
            ),
            ..cookie_agent_runtime_api::RuntimeOptions::new(directory)
        },
    })
    .await
    .expect("engine with [runtimes.local.mcp]")
}

fn hosted_fixture(engine: &crate::Engine) -> cookie_agent_protocol::McpServerInfo {
    engine
        .list_mcp_servers()
        .into_iter()
        .find(|server| server.runtime_id.as_str() == "local" && server.name == "fixture")
        .expect("the local runtime's fixture is listed")
}

#[tokio::test]
async fn the_local_runtime_hosts_its_template_mcp_servers() {
    let (fixture, selection) = custom_fixture();
    fixture.engine.shutdown().await;
    let refuse = fixture._directory.path().join("refuse");
    let engine = open_with_local_mcp(&fixture, &refuse).await;
    let workspace = engine
        .inner
        .runtimes
        .local_workspace_root()
        .to_string_lossy()
        .into_owned();

    // The server connected before the engine opened, and its tools are the
    // local runtime's.
    let listed = hosted_fixture(&engine);
    assert_eq!(listed.workspace.as_deref(), Some(workspace.as_str()));
    assert_eq!(
        listed.state,
        cookie_agent_protocol::McpServerState::Connected
    );
    assert_eq!(listed.tool_count, 2);
    assert_eq!(
        listed.definition.command.as_deref(),
        Some(crate::runtime::tests::support::python_command())
    );
    let session = engine.create_session(selection).unwrap().session_id;
    let composition = engine.compose_session_tools(session, |_, _| true).unwrap();
    let echo = composition.get("fixture_echo_text").expect("MCP tool");
    assert_eq!(
        echo.source.origin.runtime().map(|runtime| runtime.id()),
        Some("local")
    );

    // A reconnect is routed to the local runtime; a failed one reaches
    // clients with the runtime and workspace.
    let mut failures = engine.subscribe_mcp_failures();
    let mut changes = engine.subscribe_mcp_changes();
    changes.mark_unchanged();
    std::fs::write(&refuse, "").unwrap();
    let error = engine
        .reconnect_runtime_mcp_server("local", Some(&workspace), "fixture".into())
        .await
        .expect_err("the fixture refuses to start");
    assert!(error.to_string().contains("fixture"), "{error}");
    let failure = tokio::time::timeout(WAIT, failures.recv())
        .await
        .expect("the failure is reported")
        .unwrap();
    assert_eq!(
        failure,
        crate::McpServerFailure {
            runtime_id: "local".into(),
            workspace: Some(workspace.clone()),
            server: "fixture".into(),
            reason: failure.reason.clone(),
        }
    );
    tokio::time::timeout(WAIT, changes.changed())
        .await
        .expect("the change is signalled")
        .unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    while hosted_fixture(&engine).state != cookie_agent_protocol::McpServerState::Failed {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never listed failed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        engine
            .mcp_failures()
            .iter()
            .any(|failure| failure.runtime_id == "local" && failure.server == "fixture")
    );
    let deadline = tokio::time::Instant::now() + WAIT;
    while engine
        .compose_session_tools(session, |_, _| true)
        .unwrap()
        .get("fixture_echo_text")
        .is_some()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "tools never dropped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Opening a session that uses the runtime retries the failed server,
    // and its next run waits for the attempt.
    std::fs::remove_file(&refuse).unwrap();
    engine.connect_session(session).await;
    tokio::time::timeout(WAIT, engine.await_session_mcp(session))
        .await
        .expect("the retry finishes");
    let deadline = tokio::time::Instant::now() + WAIT;
    while engine
        .compose_session_tools(session, |_, _| true)
        .unwrap()
        .get("fixture_echo_text")
        .is_none()
    {
        assert!(tokio::time::Instant::now() < deadline, "tools never back");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        hosted_fixture(&engine).state,
        cookie_agent_protocol::McpServerState::Connected
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn local_mcp_servers_need_the_protocol_local_runtime() {
    let (fixture, _) = custom_fixture();
    fixture.engine.shutdown().await;
    let directory = fixture._directory.path();
    let error = crate::Engine::open(crate::EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.to_owned(),
        config: local_mcp_config(&fixture.config, &directory.join("refuse")),
        model_manager: Arc::clone(&fixture.manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory),
    })
    .err()
    .expect("an in-process local runtime hosts no MCP servers");
    assert!(
        error.to_string().contains("[runtimes.local.mcp]"),
        "{error}"
    );
}

/// Initializes against a fake runtime, as one written in any language would
/// be, that answers `runtime/initialize` with `result` plus the current
/// protocol version.
async fn initialize_fake(mut result: serde_json::Value) -> Result<crate::StartedRuntime, String> {
    result["version"] = cookie_agent_runtime_api::protocol::RUNTIME_PROTOCOL_VERSION.into();
    let (daemon, mut runtime) = in_process_pair(8);
    let fake = tokio::spawn(async move {
        let request = runtime.recv().await.unwrap().unwrap().into_text().unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "runtime/initialize");
        let reply = serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": result });
        runtime
            .send(MessageFrame::Text(reply.to_string()))
            .await
            .unwrap();
        runtime
    });
    let settings = runtime_settings(&custom_fixture().0.config, "fake");
    let initialized = RuntimeClient::initialize(
        cookie_agent_runtime_api::RuntimeConnection::new(Box::new(daemon)),
        "fake",
        None,
        &settings,
        Duration::from_secs(1),
        None,
    )
    .await
    .map_err(|error| error.to_string());
    drop(fake.await.unwrap());
    let (client, result) = initialized?;
    started(client, result)
}

fn fake_result(tools: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "kind": "browser",
        "attachment_name": "browser",
        "tools": { "tools": tools },
        "skills": { "skills": [], "discoveries": [] },
    })
}

/// A runtime of any kind needs only to speak the protocol: the daemon takes
/// its self-declared kind as a label, its workspace and platform as
/// optional, and its tools' permission actions as labels of its own.
#[tokio::test]
async fn a_runtime_of_any_kind_declares_its_own_permission_actions() {
    let started = initialize_fake(fake_result(serde_json::json!([{
        "name": "open_page",
        "permission": "browse",
        "description": "Open a page.",
        "parameters": { "type": "object" },
    }])))
    .await
    .unwrap();
    assert_eq!(started.info.kind().as_str(), "browser");
    assert!(started.info.workspace().is_none());
    let tools = started.providers[0]
        .tools_for_session(&cookie_agent_runtime_api::SessionToolContext::new(
            SessionId::new_v7(),
        ))
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].permission_name, "browse");
}

/// The unconditional permission name skips every check, so no runtime may
/// declare it.
#[tokio::test]
async fn a_runtime_cannot_declare_an_unconditional_tool() {
    let Err(error) = initialize_fake(fake_result(serde_json::json!([{
        "name": "escape",
        "permission": "unconditional",
        "description": "Run anything.",
        "parameters": { "type": "object" },
    }])))
    .await
    else {
        panic!("an unconditional tool was accepted");
    };
    assert!(error.contains("reserved"), "{error}");
}
