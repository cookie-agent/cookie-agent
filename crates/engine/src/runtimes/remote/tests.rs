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
    CodingToolsFactory, PreparedExecutor, PreparedTool, ProgressCapture, ProgressSink,
    RuntimeConnector, SessionToolContext, ToolArtifacts, ToolCall, ToolCompletion, ToolError,
    ToolExecutionContext, ToolOrigin, ToolPreparationContext, ToolProgress, ToolProvider, ToolSpec,
    TurnAgentContext, authoring,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ProtocolTemplate, RuntimeClient, runtime_settings};
use crate::runtime::tests::support::{custom_fixture, private_tempdir};

const WAIT: Duration = Duration::from_secs(20);

fn turn_context() -> Arc<TurnAgentContext> {
    Arc::new(TurnAgentContext {
        agent: cookie_agent_protocol::AgentId::new("test").expect("agent ID"),
        model: "test/model".parse().expect("model key"),
        adapter: cookie_agent_protocol::AdaptorId::OPENAI_CHAT,
        input_media: Vec::new(),
    })
}

/// A transport that can be cut, as when a runtime process dies, or
/// silenced, as a connection that died without a close looks: frames sent
/// on it vanish and none arrive, yet it never ends.
struct Breakable {
    inner: cookie_agent_protocol::InProcessStream,
    broken: CancellationToken,
    silenced: CancellationToken,
}

#[async_trait]
impl Transport for Breakable {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        if self.broken.is_cancelled() {
            return Err(TransportError::Closed);
        }
        if self.silenced.is_cancelled() {
            return Ok(());
        }
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            if self.silenced.is_cancelled() {
                self.broken.cancelled().await;
                return Ok(None);
            }
            tokio::select! {
                () = self.broken.cancelled() => return Ok(None),
                () = self.silenced.cancelled() => {}
                frame = self.inner.recv() => return frame,
            }
        }
    }
}

/// Serves a coding runtime per connection; cancelling `broken` cuts every
/// connection.
struct TestConnector {
    tools: CodingToolsFactory,
    /// The runtimes it serves declare producer messaging.
    producer_messaging: bool,
    broken: Mutex<CancellationToken>,
    silenced: Mutex<CancellationToken>,
    /// Connections made so far.
    connects: std::sync::atomic::AtomicUsize,
    /// While set, connecting fails, as when a host is unreachable.
    refuse: std::sync::atomic::AtomicBool,
}

impl TestConnector {
    fn new(tools: CodingToolsFactory) -> Arc<Self> {
        Self::with_producers(tools, false)
    }

    fn with_producers(tools: CodingToolsFactory, producer_messaging: bool) -> Arc<Self> {
        Arc::new(Self {
            tools,
            producer_messaging,
            broken: Mutex::new(CancellationToken::new()),
            silenced: Mutex::new(CancellationToken::new()),
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

    /// Silences the current connections without closing them; later
    /// connections work.
    fn silence(&self) {
        let mut silenced = self.silenced.lock().unwrap();
        silenced.cancel();
        *silenced = CancellationToken::new();
    }

    /// Silences every connection from now on, the ones made later too, as
    /// a host that accepts connections and never answers.
    fn silence_for_good(&self) {
        self.silenced.lock().unwrap().cancel();
    }
}

#[async_trait]
impl RuntimeConnector for TestConnector {
    async fn connect(&self) -> Result<cookie_agent_runtime_api::RuntimeConnection, String> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("connection refused".into());
        }
        self.connects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (daemon, runtime) = in_process_pair(64);
        tokio::spawn(serve(
            Box::new(runtime),
            ServeOptions {
                tools: Arc::clone(&self.tools),
                producer_messaging: self.producer_messaging,
                ..Default::default()
            },
        ));
        Ok(cookie_agent_runtime_api::RuntimeConnection::new(Box::new(
            Breakable {
                inner: daemon,
                broken: self.broken.lock().unwrap().clone(),
                silenced: self.silenced.lock().unwrap().clone(),
            },
        )))
    }
}

/// A started protocol runtime for `workspace` and its one provider.
async fn connect(
    connector: &Arc<TestConnector>,
    workspace: &std::path::Path,
) -> (crate::StartedRuntime, Arc<dyn ToolProvider>) {
    let connection = connector.connect().await.unwrap();
    let settings = runtime_settings(&custom_fixture().0.config, "remote");
    let (client, _) = RuntimeClient::initialize(
        connection,
        "remote",
        &settings,
        Duration::from_secs(1),
        None,
    )
    .await
    .unwrap();
    let started = client
        .open(
            &cookie_agent_protocol::CapabilityName::coding(),
            Some(workspace),
        )
        .await
        .unwrap();
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
        started.info.capability().clone(),
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

struct Call {
    context: ToolExecutionContext,
    capture: Arc<RecordingCapture>,
    artifacts: Arc<RecordingArtifacts>,
    detach: cookie_agent_runtime_api::DetachWatch,
    _display: mpsc::Receiver<ToolProgress>,
}

fn call_context(cancellation: CancellationToken) -> Call {
    let (detach_point, detach) = cookie_agent_runtime_api::DetachPoint::channel();
    let capture = Arc::new(RecordingCapture::default());
    let artifacts = Arc::new(RecordingArtifacts::default());
    let (sender, display) = mpsc::channel(256);
    Call {
        context: ToolExecutionContext {
            background: false,
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            progress: ProgressSink::with_capture(sender, capture.clone()),
            cancellation,
            turn_context: turn_context(),
            artifacts: artifacts.clone(),
            detach: detach_point,
            producers: None,
        },
        capture,
        artifacts,
        detach,
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
async fn streamed_output_crosses_the_protocol() {
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
            "command": "echo got:hello; echo diagnostics >&2",
        }),
    )
    .await;
    let declaration = prepared.declaration().expect("reported metadata").clone();
    assert_eq!(declaration.permission_name.as_deref(), Some("bash"));
    assert_eq!(
        declaration.permission_resource.as_deref(),
        Some("echo got:hello; echo diagnostics >&2")
    );
    // The runtime reports the tool's own presentation: its human title,
    // hint, and argument blocks.
    assert_eq!(
        declaration.presentation.title.as_str(),
        "Run `echo got:hello; echo diagnostics >&2`"
    );
    assert_eq!(
        declaration
            .presentation
            .hint
            .as_ref()
            .map(|hint| hint.as_str()),
        Some(cookie_agent_protocol::ToolHint::SHELL)
    );
    assert_eq!(declaration.presentation.arguments.len(), 1);
    // The daemon builds the approval identity from what the runtime declared.
    assert!(prepared.is_declared());
    let identified = crate::testing::identify(
        provider.as_ref(),
        "bash",
        &ToolOrigin::Runtime(Arc::new(started_info(&started))),
        prepared,
    )
    .unwrap();
    assert_eq!(
        identified.policy_labels(),
        std::slice::from_ref(&declaration.permission_resource)
    );
    assert_eq!(
        identified.operation().resources()[0].capability,
        cookie_agent_protocol::PermissionAction::BASH
    );
    let prepared = identified;
    // The sync metadata methods defer to preparation.
    assert!(
        provider
            .get_permission_resource("bash", &serde_json::json!({"command": "true"}))
            .is_err()
    );

    let call = call_context(CancellationToken::new());
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
    let call = call_context(cancellation.clone());
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
        Ok(completion) => assert!(completion.failed || !completion.result.display.is_empty()),
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
            background: false,
            background_default: false,
            name: "snapshot".into(),
            collision: Default::default(),
            permission_name: Some("read".into()),
            description: "Attach a snapshot.".into(),
            parameters: authoring::schema::<NoArgs>(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            output: Default::default(),
        }])
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Ok(Some("read"))
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((Some("read"), Some("snapshot".into())))
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
            display: Vec::new(),
            background: None,
            retained_output: None,
            metadata: serde_json::Value::Null,
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
    let call = call_context(CancellationToken::new());
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
async fn a_remote_call_announces_its_detach_point() {
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let (started, provider) = connect(&connector, workspace.path()).await;
    let prepared = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({"command": "sleep 0.2; echo done"}),
    )
    .await;
    let mut call = call_context(CancellationToken::new());
    let (completion, detachable) = tokio::join!(execute(prepared, call.context), async {
        tokio::time::timeout(WAIT, call.detach.reached())
            .await
            .expect("tool/detachable reaches the daemon")
    });
    assert!(!completion.unwrap().failed);
    let note = detachable.note.expect("bash names its process");
    assert!(note.starts_with("Process "), "{note}");
    assert_eq!(call.capture.stream("stdout"), "done\n");
}

#[tokio::test]
async fn transport_loss_fails_calls_in_flight() {
    let workspace = private_tempdir();
    let connector = TestConnector::builtin();
    let (started, provider) = connect(&connector, workspace.path()).await;
    let running = prepare(
        &provider,
        &started,
        "bash",
        serde_json::json!({"command": "echo started; sleep 30"}),
    )
    .await;
    let call = call_context(CancellationToken::new());
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
        .ensure("remote", None, Some(workspace.path()))
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

/// One connection serves every workspace of a runtime: a second workspace
/// opens over it, and a dropped connection is replaced once for both.
#[tokio::test]
async fn one_connection_serves_every_workspace_and_reconnects_once_for_them() {
    let (fixture, _) = custom_fixture();
    let (api, web) = (private_tempdir(), private_tempdir());
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let registry = &fixture.engine.inner.runtimes;
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(20),
    ]));
    let first = registry
        .ensure("remote", None, Some(api.path()))
        .await
        .unwrap();
    let second = registry
        .ensure("remote", None, Some(web.path()))
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_ne!(
        first.info().workspace().unwrap().root(),
        second.info().workspace().unwrap().root()
    );
    assert_eq!(connector.connects(), 1);
    let (first_connection, second_connection) =
        (first.connection().unwrap(), second.connection().unwrap());
    assert_eq!(first_connection.epoch(), second_connection.epoch());
    assert_ne!(first_connection.instance, second_connection.instance);
    let listing = registry
        .templates()
        .into_iter()
        .find(|template| template.runtime_id == "remote")
        .unwrap()
        .listing;
    assert!(listing.connected);
    assert_eq!(listing.capabilities[0].name.as_str(), "coding");

    connector.cut();
    tokio::time::timeout(WAIT, async {
        while [&first, &second].iter().any(|instance| {
            instance
                .connection()
                .is_none_or(|connection| connection.epoch() == first_connection.epoch())
        }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both instances reconnect");
    assert_eq!(connector.connects(), 2);
    assert_eq!(
        first.connection().unwrap().epoch(),
        second.connection().unwrap().epoch()
    );
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
        .ensure("remote", None, Some(workspace.path()))
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
            cookie_agent_protocol::paths::canonicalize(workspace.path())
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
        .ensure("remote", None, Some(workspace.path()))
        .await
        .err()
        .expect("still unreachable");
    assert!(error.to_string().contains("connection refused"), "{error}");
    connector.set_refuse(false);
    let revived = registry
        .ensure("remote", None, Some(workspace.path()))
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
    tokio::time::timeout(WAIT, connection.client.closed())
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
    let instance = registry
        .ensure("remote", None, Some(&first_link))
        .await
        .unwrap();
    assert_eq!(
        instance.info().workspace().unwrap().root(),
        canonical.as_path()
    );
    assert_eq!(connector.connects(), 1);
    // The same spelling and the canonical path reuse it without connecting.
    for path in [&first_link, &canonical] {
        let again = registry.ensure("remote", None, Some(path)).await.unwrap();
        assert!(Arc::ptr_eq(&again, &instance));
    }
    assert_eq!(connector.connects(), 1);
    // A new spelling asks the runtime where it resolves, over the same
    // connection, and the running instance is shared.
    let deduplicated = registry
        .ensure("remote", None, Some(&second_link))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&deduplicated, &instance));
    assert_eq!(connector.connects(), 1);
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
            None,
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
        scope: None,
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
    // Its calls' presentations name the MCP server.
    assert_eq!(
        echo.source.provider.tool_source(&echo.spec.name),
        Some(cookie_agent_runtime_api::ProvidedToolSource::Mcp {
            server: "fixture".into()
        })
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

/// A runtime that hosts servers its own machine defines, as a dial-in
/// runtime hosts its `[runtimes.local.mcp]`, lists them with the source
/// `machine` and an empty definition: the daemon never sees them.
#[tokio::test]
async fn servers_the_runtimes_machine_defines_are_listed_as_machine_servers() {
    let (fixture, _) = custom_fixture();
    fixture.engine.shutdown().await;
    let directory = fixture._directory.path();
    let machine_config = local_mcp_config(&fixture.config, &directory.join("refuse"));
    let machine_servers = machine_config
        .runtimes
        .iter()
        .find(|(id, _)| id.as_str() == "local")
        .expect("local template")
        .1
        .mcp_servers
        .iter()
        .map(
            |(name, server)| cookie_agent_runtime_api::protocol::McpServerSettings {
                name: name.clone(),
                config: serde_json::to_value(server).unwrap(),
            },
        )
        .collect();
    let engine = crate::Engine::open_async(crate::EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions {
            local: cookie_agent_runtime_api::LocalRuntime::Protocol(
                cookie_agent_coding_runtime::server::in_process_connector_with_options(
                    ServeOptions {
                        mcp_servers: machine_servers,
                        ..ServeOptions::default()
                    },
                ),
            ),
            ..cookie_agent_runtime_api::RuntimeOptions::new(directory)
        },
    })
    .await
    .expect("engine whose local runtime hosts its machine's servers");
    let listed = hosted_fixture(&engine);
    assert_eq!(
        listed.source,
        cookie_agent_protocol::McpConfigSource::Machine
    );
    assert_eq!(listed.definition.command, None);
    assert_eq!(listed.definition.url, None);
    assert!(listed.definition.enabled);
    assert_eq!(
        listed.state,
        cookie_agent_protocol::McpServerState::Connected
    );
    assert_eq!(listed.tool_count, 2);
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
/// be, that offers one capability, `browse`, of kind `browser` without a
/// workspace, and answers `capability/open` with `opened`.
async fn initialize_fake(opened: serde_json::Value) -> Result<crate::StartedRuntime, String> {
    initialize_fake_with(fake_hello(), opened).await
}

/// The fake runtime's initialize reply.
fn fake_hello() -> serde_json::Value {
    serde_json::json!({
        "protocol_version": cookie_agent_protocol::RUNTIME_PROTOCOL_VERSION,
        "capabilities": [{
            "name": "browse",
            "kind": "browser",
            "takes_workspace": false,
        }],
    })
}

/// [`initialize_fake`] with the initialize reply `hello`.
async fn initialize_fake_with(
    hello: serde_json::Value,
    opened: serde_json::Value,
) -> Result<crate::StartedRuntime, String> {
    let (daemon, mut runtime) = in_process_pair(8);
    let fake = tokio::spawn(async move {
        for reply in [hello, opened] {
            let Ok(Some(request)) = runtime.recv().await else {
                return runtime;
            };
            let request: serde_json::Value =
                serde_json::from_str(&request.into_text().unwrap()).unwrap();
            let reply =
                serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "result": reply });
            runtime
                .send(MessageFrame::Text(reply.to_string()))
                .await
                .unwrap();
        }
        runtime
    });
    let settings = runtime_settings(&custom_fixture().0.config, "fake");
    let initialized = RuntimeClient::initialize(
        cookie_agent_runtime_api::RuntimeConnection::new(Box::new(daemon)),
        "fake",
        &settings,
        Duration::from_secs(1),
        None,
    )
    .await
    .map_err(|error| error.to_string());
    let opened = match initialized {
        Ok((client, _)) => client
            .open(
                &cookie_agent_protocol::CapabilityName::new("browse").unwrap(),
                None,
            )
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error),
    };
    drop(fake.await.unwrap());
    opened
}

fn fake_result(tools: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "instance": "browse_1",
        "attachment_name": "browser",
        "tools": { "tools": tools },
        "skills": { "skills": [], "discoveries": [] },
    })
}

/// A runtime may report its home directory (runtime protocol 1.3.0): its
/// instances then carry it, for permission rules to expand `~` against. One
/// that reports none, or one that is not an absolute path, has no home.
#[tokio::test]
async fn an_instance_carries_the_home_its_runtime_reported() {
    let opened = || fake_result(serde_json::json!([]));
    let with_home = |home: serde_json::Value| {
        let mut hello = fake_hello();
        hello["home"] = home;
        hello
    };
    for (hello, expected) in [
        (with_home(serde_json::json!("/home/ada")), Some("/home/ada")),
        (
            with_home(serde_json::json!(r"C:\Users\ada")),
            Some(r"C:\Users\ada"),
        ),
        (fake_hello(), None),
        (with_home(serde_json::json!("relative/home")), None),
        (with_home(serde_json::json!("/home/a\nda")), None),
    ] {
        let started = initialize_fake_with(hello.clone(), opened()).await.unwrap();
        assert_eq!(started.info.home(), expected, "{hello}");
    }
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
    assert_eq!(tools[0].permission_name.as_deref(), Some("browse"));
}

/// A runtime of another major is refused from its reply's version alone,
/// with both versions named; one of another minor or patch connects.
#[tokio::test]
async fn a_runtime_of_another_version_is_refused_with_unsupported_protocol() {
    let daemon = cookie_agent_protocol::RUNTIME_PROTOCOL_VERSION;
    let next_major = cookie_agent_protocol::SemVer::new(daemon.major() + 1, 0, 0);
    for (version, named) in [
        (serde_json::json!(next_major), next_major.to_string()),
        // A build from before the freeze.
        (serde_json::json!("0.8.0"), "0.8.0".to_owned()),
        (serde_json::json!(6), "6".to_owned()),
    ] {
        // The rest of the reply is a shape this build does not share.
        let hello = serde_json::json!({"protocol_version": version, "capabilities": "many"});
        let error = initialize_fake_with(hello, fake_result(serde_json::json!([])))
            .await
            .err()
            .expect("another version");
        assert!(
            error.contains("runtime `fake`")
                && error.contains(&format!("{daemon} here and {named}")),
            "{error}"
        );
    }
    for (minor, patch) in [(0, 1), (2, 0), (5, 3)] {
        let mut hello = fake_hello();
        hello["protocol_version"] = serde_json::json!(cookie_agent_protocol::SemVer::new(
            daemon.major(),
            daemon.minor() + minor,
            daemon.patch() + patch
        ));
        initialize_fake_with(hello, fake_result(serde_json::json!([])))
            .await
            .expect("another minor or patch connects");
    }
}

/// A runtime may declare a tool that needs no permission by leaving out its
/// `permission`.
#[tokio::test]
async fn a_runtime_can_declare_a_permission_free_tool() {
    let started = initialize_fake(fake_result(serde_json::json!([{
        "name": "status",
        "description": "Report status.",
        "parameters": { "type": "object" },
    }])))
    .await
    .unwrap();
    let tools = started.providers[0]
        .tools_for_session(&cookie_agent_runtime_api::SessionToolContext::new(
            SessionId::new_v7(),
        ))
        .unwrap();
    assert_eq!(tools[0].permission_name, None);
}

/// A runtime tool that registers a producer for the call's session, or for
/// the session its arguments name, and sends one message through it.
struct ReportTool;

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    #[serde(default)]
    session: Option<SessionId>,
    /// Detach as work that reports itself first, then send the message a
    /// moment later unless the call is cancelled meanwhile.
    #[serde(default)]
    detach_first: bool,
}

#[async_trait]
impl ToolProvider for ReportTool {
    fn provider_id(&self) -> &'static str {
        "test.report"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            background: false,
            background_default: false,
            name: "report".into(),
            collision: Default::default(),
            permission_name: None,
            description: "Report a finished job.".into(),
            parameters: authoring::schema::<ReportArgs>(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            output: Default::default(),
        }])
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Ok(None)
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((None, None))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("report".into())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: ReportArgs = authoring::parse_args("report", call.arguments.clone())?;
        // No rule is evaluated; the capability only names the operation.
        let action = cookie_agent_protocol::PermissionAction::new("report")
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let resource = authoring::prepared_resource(
            action.clone(),
            "report",
            b"report",
            b"report",
            cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
            cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = authoring::prepared_operation(
            "report",
            &call.arguments,
            vec![(action, "report")],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(ReportExecutor(args)),
        )
    }
}

struct ReportExecutor(ReportArgs);

#[async_trait]
impl PreparedExecutor for ReportExecutor {
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let producers = context
            .producers
            .clone()
            .ok_or_else(|| ToolError::execution("no producer channel"))?;
        let session = self.0.session.unwrap_or(context.session);
        let (key, body) = if self.0.detach_first {
            context
                .detach
                .reached_with(cookie_agent_runtime_api::Detachable {
                    note: None,
                    reports_itself: true,
                });
            tokio::select! {
                () = context.cancellation.cancelled() => {
                    return Err(ToolError::execution("cancelled"));
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            ("job-2", "late output")
        } else {
            ("job-1", "job output")
        };
        let sent = async {
            let producer = producers
                .register(
                    session,
                    cookie_agent_protocol::ProducerName::new("jobs").unwrap(),
                )
                .await?;
            producers
                .send(
                    session,
                    producer,
                    cookie_agent_runtime_api::ProducerMessage {
                        mode: cookie_agent_protocol::ProducerDeliveryMode::Queue,
                        idempotency_key: cookie_agent_protocol::ProducerIdempotencyKey::new(key)
                            .unwrap(),
                        description: cookie_agent_protocol::SafeDisplayText::new("Job finished")
                            .unwrap(),
                        body: body.into(),
                    },
                )
                .await
        }
        .await;
        Ok(ToolCompletion::single(PersistedToolResult {
            title: cookie_agent_protocol::SafeDisplayText::new("Report").unwrap(),
            output: match sent {
                Ok(message) => format!("sent {message}"),
                Err(error) => format!("error: {error}"),
            },
            display: Vec::new(),
            background: None,
            retained_output: None,
            metadata: serde_json::Value::Null,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        }))
    }
}

/// Prepares the `report` tool of `instance` for `session` with `arguments`.
async fn prepare_report(
    instance: &crate::runtimes::RuntimeInstance,
    session: SessionId,
    arguments: serde_json::Value,
) -> PreparedTool {
    let provider = Arc::clone(&instance.sources().1[0]);
    provider
        .prepare(
            ToolPreparationContext {
                session,
                run: RunId::new_v7(),
                origin: instance.origin(),
                turn_context: turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "report".into(),
                arguments,
            },
        )
        .await
        .expect("prepared")
}

/// Runs the `report` tool of `instance` for `session` with `arguments`.
async fn report(
    instance: &crate::runtimes::RuntimeInstance,
    session: SessionId,
    arguments: serde_json::Value,
) -> String {
    let prepared = prepare_report(instance, session, arguments).await;
    let mut call = call_context(CancellationToken::new());
    call.context.session = session;
    let completion = execute(prepared, call.context).await.expect("report runs");
    match completion.output {
        cookie_agent_protocol::ToolCompletionOutput::Single { text } => text,
        other => panic!("unexpected output {other:?}"),
    }
}

#[tokio::test]
async fn a_producing_runtime_sends_only_to_the_sessions_attached_to_it() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let connector = TestConnector::with_producers(
        Arc::new(|_workspace: &cookie_agent_runtime_api::WorkspaceContext| {
            vec![Arc::new(ReportTool) as Arc<dyn ToolProvider>]
        }),
        true,
    );
    register_remote(&fixture, &connector);
    let engine = &fixture.engine;
    let origin = cookie_agent_protocol::EventOrigin::new("user").unwrap();
    let attached = engine.create_session(selection.clone()).unwrap().session_id;
    let unattached = engine.create_session(selection).unwrap().session_id;
    let attachment = engine
        .attach_runtime(
            attached,
            "remote",
            None,
            Some(workspace.path().to_owned()),
            origin.clone(),
        )
        .await
        .unwrap();
    let instance = engine.inner.runtimes.instance(&attachment).unwrap();
    let source = cookie_agent_protocol::ProducerSource::runtime(instance.info().id()).unwrap();

    // The daemon opens producer messaging and the runtime recovers.
    tokio::time::timeout(WAIT, async {
        loop {
            let sources = engine
                .session_producers(cookie_agent_protocol::SessionProducersParams {
                    session_id: attached,
                })
                .await
                .unwrap()
                .sources;
            if sources.iter().any(|state| {
                state.source == source
                    && state.status == cookie_agent_protocol::ProducerSourceStatus::Ready
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the runtime's producer source becomes ready");

    let output = report(&instance, attached, serde_json::json!({})).await;
    assert!(output.starts_with("sent "), "{output}");
    let accepted = engine
        .inner
        .store
        .get(attached)
        .unwrap()
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::ProducerMessageAccepted {
                producer_owner,
                body,
                ..
            } => Some((producer_owner.clone(), body.as_plain_text().into_owned())),
            _ => None,
        })
        .expect("the message is accepted");
    assert_eq!(
        accepted,
        (
            cookie_agent_protocol::ProducerOwner::external(
                source.clone(),
                cookie_agent_protocol::ProducerName::new("jobs").unwrap()
            ),
            "job output".to_owned()
        )
    );

    // A session the runtime is not attached to is out of its reach.
    let refused = report(
        &instance,
        attached,
        serde_json::json!({"session": unattached}),
    )
    .await;
    assert!(
        refused.starts_with("error:") && refused.contains("not attached"),
        "{refused}"
    );

    // A call that reports itself keeps running in the runtime after the
    // daemon stops waiting for it.
    let prepared = prepare_report(
        &instance,
        attached,
        serde_json::json!({"detach_first": true}),
    )
    .await;
    let mut call = call_context(CancellationToken::new());
    call.context.session = attached;
    // Boxed, so dropping it drops the call as the daemon does.
    let mut execution = Box::pin(execute(prepared, call.context));
    tokio::select! {
        _ = &mut execution => panic!("the call detaches before it ends"),
        _ = call.detach.reached() => {}
    }
    drop(execution);
    tokio::time::timeout(WAIT, async {
        loop {
            let arrived = engine
                .inner
                .store
                .get(attached)
                .unwrap()
                .log
                .events()
                .iter()
                .any(|event| {
                    matches!(
                        &event.payload,
                        cookie_agent_protocol::EventPayload::ProducerMessageAccepted { body, .. }
                            if body == "late output"
                    )
                });
            if arrived {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the self-reporting call sends its message after the daemon stopped waiting");

    // Detaching the runtime drops its registrations there.
    let producers = |session| async move {
        engine
            .session_producers(cookie_agent_protocol::SessionProducersParams {
                session_id: session,
            })
            .await
            .unwrap()
    };
    assert_eq!(producers(attached).await.producers.len(), 2);
    // The message woke the idle session; detaching waits for that run.
    crate::runtime::tests::support::wait_for_session_not_running(engine, attached).await;
    engine
        .detach_runtime(attached, attachment.name.clone(), origin)
        .await
        .unwrap();
    let after = producers(attached).await;
    assert!(after.producers.is_empty(), "{after:?}");
    assert!(after.sources.iter().all(|state| state.source != source));
    fixture.engine.shutdown().await;
}

/// A runtime that dials in serves sessions until its connection drops; its
/// instances fail meanwhile and come back when it dials in again.
#[tokio::test]
async fn a_dial_in_runtime_serves_sessions_and_comes_back_after_a_drop() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let engine = &fixture.engine;
    let registry = &engine.inner.runtimes;
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(20),
    ]));
    let connector = TestConnector::builtin();
    let dial_in = || async {
        let connection = connector.connect().await.unwrap();
        engine
            .accept_dial_in_runtime("labpc", connection.transport)
            .await
    };
    dial_in().await.expect("labpc dials in");
    let listing = || {
        engine
            .runtime_templates()
            .into_iter()
            .find(|template| template.runtime_id == "labpc")
            .expect("labpc listed")
    };
    assert_eq!(
        listing().lifetime,
        cookie_agent_protocol::RuntimeLifetime::DialIn
    );
    assert!(listing().listing.connected);
    assert_eq!(listing().listing.capabilities[0].name.as_str(), "coding");

    // One connection per ID, refused for now; reserved, configured, and
    // malformed IDs are refused for good.
    let error = dial_in().await.expect_err("already connected");
    assert_eq!(error, crate::DialInRefusal::Connected("labpc".into()));
    assert_eq!(error.to_string(), "runtime `labpc` is already connected");
    assert!(!error.is_permanent());
    register_remote(&fixture, &connector);
    for (runtime_id, refusal, message) in [
        (
            "local",
            crate::DialInRefusal::Reserved("local".into()),
            "runtime ID `local` is reserved for the daemon's built-in runtime",
        ),
        (
            "global",
            crate::DialInRefusal::Reserved("global".into()),
            "runtime ID `global` is reserved for the daemon's built-in runtime",
        ),
        (
            "remote",
            crate::DialInRefusal::Configured("remote".into()),
            "runtime `remote` is configured on the daemon ([runtimes.remote]) and cannot dial in",
        ),
        (
            "Lab-PC",
            crate::DialInRefusal::InvalidId("Lab-PC".into()),
            "invalid runtime ID `Lab-PC`: it must match [a-z][a-z0-9_]{0,31}",
        ),
    ] {
        let error = engine
            .claim_dial_in_runtime(runtime_id)
            .err()
            .unwrap_or_else(|| panic!("`{runtime_id}` cannot dial in"));
        assert_eq!(error, refusal);
        assert_eq!(error.to_string(), message);
        assert!(error.is_permanent(), "{error}");
    }
    // A claim holds the ID while its runtime is being accepted.
    let claim = engine
        .claim_dial_in_runtime("devbox")
        .expect("devbox claims");
    assert_eq!(
        engine.claim_dial_in_runtime("devbox").err(),
        Some(crate::DialInRefusal::Connected("devbox".into()))
    );
    drop(claim);
    drop(engine.claim_dial_in_runtime("devbox").expect("released"));

    let session = engine.create_session(selection).unwrap().session_id;
    let origin = cookie_agent_protocol::EventOrigin::new("user").unwrap();
    let attachment = engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(workspace.path().to_owned()),
            origin,
        )
        .await
        .expect("attach labpc");
    assert_eq!(attachment.capability.as_str(), "coding");

    // The connection drops: the instance fails once reconnecting finds no
    // connection, and the runtime shows as disconnected.
    connector.cut();
    wait_for_status(registry, "labpc", crate::RuntimeInstanceStatus::Failed).await;
    assert!(!listing().listing.connected);
    assert!(registry.instance(&attachment).is_none());

    // Dialing in again reopens it.
    dial_in().await.expect("labpc dials in again");
    wait_for_status(registry, "labpc", crate::RuntimeInstanceStatus::Ready).await;
    let instance = registry.instance(&attachment).expect("revived");
    assert!(!instance.connection().unwrap().is_closed());
    fixture.engine.shutdown().await;
}

/// A runtime that reports the daemon's machine ID joins the daemon's
/// machine as it connects, and the machine is written at once.
#[tokio::test]
async fn a_runtime_joins_the_machine_of_the_id_it_reports() {
    let (fixture, _) = custom_fixture();
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let daemon = fixture
        .engine
        .runtime_machine("local")
        .expect("the daemon's machine");
    assert_eq!(fixture.engine.runtime_machine("remote"), None);
    fixture
        .engine
        .inner
        .runtimes
        .ensure("remote", None, Some(fixture._directory.path()))
        .await
        .unwrap();
    let listing = fixture
        .engine
        .runtime_templates()
        .into_iter()
        .find(|template| template.runtime_id == "remote")
        .unwrap()
        .listing;
    assert_eq!(
        listing.machine,
        cookie_agent_protocol::runtime_protocol::RuntimeMachine::this_machine()
    );
    // The coding runtime reports its user's home, canonical.
    assert_eq!(
        listing.home.map(std::path::PathBuf::from),
        cookie_agent_protocol::paths::canonical_home_dir().ok()
    );
    assert_eq!(
        fixture.engine.runtime_machine("remote"),
        Some(daemon.clone())
    );
    let machines = fixture.engine.machines();
    assert_eq!(machines.len(), 1);
    assert_eq!(
        machines[0]
            .connected
            .iter()
            .map(|runtime| (runtime.runtime_id.as_str(), runtime.assigned_by))
            .collect::<Vec<_>>(),
        [
            (
                "global",
                cookie_agent_protocol::MachineAssignment::MachineId
            ),
            ("local", cookie_agent_protocol::MachineAssignment::MachineId),
            (
                "remote",
                cookie_agent_protocol::MachineAssignment::MachineId
            ),
        ]
    );
    let written =
        std::fs::read_to_string(fixture._directory.path().join("data").join("machines.json"))
            .unwrap();
    assert!(written.contains(&format!("\"{daemon}\"")), "{written}");
    fixture.engine.shutdown().await;
}

/// Prepares a call of `name` on `instance`'s runtime tools.
async fn prepare_on(
    instance: &crate::runtimes::RuntimeInstance,
    name: &str,
    arguments: serde_json::Value,
) -> Result<PreparedTool, ToolError> {
    instance.providers()[0]
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                origin: instance.origin(),
                turn_context: turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: name.into(),
                arguments,
            },
        )
        .await
}

fn labpc_listing(engine: &crate::Engine) -> crate::RuntimeListingState {
    engine
        .runtime_templates()
        .into_iter()
        .find(|template| template.runtime_id == "labpc")
        .expect("labpc listed")
        .listing
}

/// A dial-in runtime whose connection goes silent without closing, as a
/// machine that sleeps or drops off the network leaves it, is found dead
/// within the liveness window: a call being prepared fails at its deadline,
/// one executing fails once the connection is ended, both as
/// `runtime_unavailable`, the runtime shows as disconnected, and creating,
/// attaching, and delegating with it are refused at once.
#[tokio::test(start_paused = true)]
async fn a_silent_dial_in_runtime_is_dropped_and_then_refused_at_once() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let engine = &fixture.engine;
    let registry = &engine.inner.runtimes;
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(20),
    ]));
    let connector = TestConnector::builtin();
    let connection = connector.connect().await.unwrap();
    engine
        .accept_dial_in_runtime("labpc", connection.transport)
        .await
        .expect("labpc dials in");
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let origin = cookie_agent_protocol::EventOrigin::new("user").unwrap();
    let attachment = engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(workspace.path().to_owned()),
            origin.clone(),
        )
        .await
        .expect("attach labpc");
    let instance = registry.instance(&attachment).expect("ready");
    let prepared = prepare_on(
        &instance,
        "bash",
        serde_json::json!({"command": "sleep 600"}),
    )
    .await
    .expect("prepared while connected");

    connector.silence();
    let silenced = tokio::time::Instant::now();
    let call = call_context(CancellationToken::new());
    let executor = prepared.executor().lock().await.take().expect("executor");
    let execution = tokio::spawn(executor.execute(call.context));
    // The incident's subagent hung here, on `tool/prepare`, for good.
    let error = prepare_on(&instance, "bash", serde_json::json!({"command": "true"}))
        .await
        .err()
        .expect("a silent runtime prepares nothing");
    assert!(
        matches!(&error, ToolError::RuntimeUnavailable(message)
            if message == "runtime `labpc` is unavailable: it did not answer `tool/prepare` within 30 s"),
        "{error:?}"
    );
    assert_eq!(silenced.elapsed(), Duration::from_secs(30));
    // The missed deadline checks the connection at once, which ends it.
    let error = execution
        .await
        .unwrap()
        .expect_err("the execution fails once the connection is ended");
    assert!(
        matches!(&error, ToolError::RuntimeUnavailable(message)
            if message == "runtime `labpc` is unavailable: it did not answer a liveness check within 10 s"),
        "{error:?}"
    );
    assert_eq!(silenced.elapsed(), Duration::from_secs(40));
    wait_for_status(registry, "labpc", crate::RuntimeInstanceStatus::Failed).await;
    let listing = labpc_listing(engine);
    assert!(!listing.connected);
    assert_eq!(
        listing.message.as_deref(),
        Some("runtime `labpc` disconnected: it did not answer a liveness check within 10 s")
    );

    // Now every way of using it is refused at once, naming it and when it
    // was last seen.
    let refused = tokio::time::Instant::now();
    let not_connected = |error: String| {
        assert!(
            error.contains("runtime `labpc` is not connected (last seen 20"),
            "{error}"
        );
    };
    let other = private_tempdir();
    let error = engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(other.path().to_owned()),
            origin.clone(),
        )
        .await
        .expect_err("attach refused");
    not_connected(error.to_string());
    let error = engine
        .create_session_with_permissions(
            selection.clone(),
            None,
            Vec::new(),
            crate::SessionRuntimes {
                runtimes: Some(vec![crate::SessionRuntimeRequest {
                    runtime_id: "labpc".into(),
                    capability: None,
                    workspace: Some(workspace.path().to_owned()),
                }]),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect_err("creation refused");
    not_connected(error.to_string());
    let root = engine.inner.store.get(session).unwrap();
    let error = engine
        .open_delegated_capabilities(
            &root,
            &[crate::DelegatedCapability {
                runtime_id: cookie_agent_protocol::RuntimeId::new("labpc").unwrap(),
                capability: cookie_agent_protocol::CapabilityName::coding(),
                workspace: Some(workspace.path().to_string_lossy().into_owned()),
            }],
        )
        .await
        .expect_err("delegation refused");
    not_connected(error.to_string());
    assert_eq!(refused.elapsed(), Duration::ZERO, "refused without waiting");
    fixture.engine.shutdown().await;
}

/// An instance whose connection has gone quiet is checked before a session
/// gets it: one that does not answer is not handed out, and its dial-in
/// runtime is refused as not connected.
#[tokio::test(start_paused = true)]
async fn an_instance_of_a_dead_connection_is_not_handed_out() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let engine = &fixture.engine;
    let connector = TestConnector::builtin();
    let connection = connector.connect().await.unwrap();
    engine
        .accept_dial_in_runtime("labpc", connection.transport)
        .await
        .expect("labpc dials in");
    let session = engine.create_session(selection).unwrap().session_id;
    let origin = cookie_agent_protocol::EventOrigin::new("user").unwrap();
    let attach = || {
        engine.attach_runtime(
            session,
            "labpc",
            None,
            Some(workspace.path().to_owned()),
            origin.clone(),
        )
    };
    attach().await.expect("attach labpc");
    connector.silence();
    // Quiet for longer than a live connection ever is, yet not long enough
    // for the liveness watch to have ended it.
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(labpc_listing(engine).connected);
    let checked = tokio::time::Instant::now();
    let error = attach()
        .await
        .expect_err("the instance of a silent connection is not reused");
    assert!(
        error
            .to_string()
            .starts_with("runtime error: runtime `labpc` is not connected (last seen "),
        "{error}"
    );
    assert_eq!(checked.elapsed(), Duration::from_secs(10));
    assert!(!labpc_listing(engine).connected);
    fixture.engine.shutdown().await;
}

/// A dial-in runtime dialing in again while its old connection still looks
/// open gets in as soon as the old one is found dead, which its attempt
/// checks at once.
#[tokio::test(start_paused = true)]
async fn dialing_in_again_checks_the_old_connection_at_once() {
    let (fixture, _) = custom_fixture();
    let engine = &fixture.engine;
    let connector = TestConnector::builtin();
    let dial_in = || async {
        let connection = connector.connect().await.unwrap();
        engine
            .accept_dial_in_runtime("labpc", connection.transport)
            .await
    };
    dial_in().await.expect("labpc dials in");
    connector.silence();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let attempt = tokio::time::Instant::now();
    assert_eq!(
        dial_in().await.expect_err("the old connection looks open"),
        crate::DialInRefusal::Connected("labpc".into())
    );
    tokio::time::timeout(Duration::from_secs(11), async {
        while labpc_listing(engine).connected {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the old connection is ended within the probe's 10 s");
    assert!(attempt.elapsed() <= Duration::from_secs(11));
    dial_in().await.expect("labpc dials in again");
    assert!(labpc_listing(engine).connected);
    fixture.engine.shutdown().await;
}

/// A runtime the daemon starts must come up before a session gets it: one
/// that cannot be reached, or that never answers its handshake, refuses
/// creating the session, which then does not exist.
#[tokio::test(start_paused = true)]
async fn a_runtime_that_does_not_start_refuses_creating_the_session() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let engine = &fixture.engine;
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let sessions = || engine.inner.store.list_roots().len();
    let before = sessions();
    let create = || {
        engine.create_session_with_permissions(
            selection.clone(),
            None,
            Vec::new(),
            crate::SessionRuntimes {
                runtimes: Some(vec![crate::SessionRuntimeRequest {
                    runtime_id: "remote".into(),
                    capability: None,
                    workspace: Some(workspace.path().to_owned()),
                }]),
                global: true,
                cwd: None,
            },
        )
    };
    connector.set_refuse(true);
    let error = create().await.expect_err("an unreachable runtime");
    assert_eq!(
        error.to_string(),
        "runtime error: runtime `remote` failed to start: connection refused"
    );
    connector.set_refuse(false);
    connector.silence_for_good();
    let started = tokio::time::Instant::now();
    let error = create().await.expect_err("a runtime that never answers");
    assert_eq!(
        error.to_string(),
        "runtime error: runtime `remote` failed to start: the runtime did not start within \
         20000 ms"
    );
    assert_eq!(started.elapsed(), WAIT);
    assert_eq!(sessions(), before, "a refused creation creates nothing");
    assert!(
        engine
            .runtime_instances()
            .iter()
            .all(|instance| instance.runtime_id != "remote"),
        "nor records an instance"
    );
    fixture.engine.shutdown().await;
}

/// An instance of a runtime the daemon starts whose connection went dead
/// without closing, as a hung `ssh` leaves it, is not handed out: the
/// session waits for the daemon to start the runtime again and gets the
/// instance over the new connection.
#[tokio::test(start_paused = true)]
async fn a_session_waits_for_a_dead_spawned_runtime_to_come_back() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let engine = &fixture.engine;
    let registry = &engine.inner.runtimes;
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(20),
    ]));
    let connector = TestConnector::builtin();
    register_remote(&fixture, &connector);
    let origin = cookie_agent_protocol::EventOrigin::new("user").unwrap();
    let attach = || async {
        let session = engine.create_session(selection.clone()).unwrap().session_id;
        engine
            .attach_runtime(
                session,
                "remote",
                None,
                Some(workspace.path().to_owned()),
                origin.clone(),
            )
            .await
    };
    let first = attach().await.expect("attach remote");
    let instance = registry.instance(&first).unwrap();
    let old = instance.connection().unwrap();
    connector.silence();
    tokio::time::sleep(Duration::from_secs(20)).await;
    attach().await.expect("attached once it came back");
    assert_eq!(connector.connects(), 2);
    assert!(old.is_closed(), "the dead connection was ended");
    let connection = registry.instance(&first).unwrap().connection().unwrap();
    assert!(!connection.is_closed());
    assert_ne!(connection.epoch(), old.epoch());
    fixture.engine.shutdown().await;
}

/// A quiet connection is pinged and stays up as long as its runtime
/// answers; one that goes silent without closing is ended once nothing has
/// arrived for 45 s, with no call needed to notice, and its instance is
/// reconnected, then failed.
#[tokio::test(start_paused = true)]
async fn a_quiet_runtime_stays_up_and_a_silent_one_is_ended_within_the_window() {
    let (fixture, selection) = custom_fixture();
    let workspace = private_tempdir();
    let engine = &fixture.engine;
    let registry = &engine.inner.runtimes;
    registry.set_reconnect_policy(crate::runtimes::ReconnectPolicy::with_delays(vec![
        Duration::from_millis(20),
    ]));
    let connector = TestConnector::builtin();
    let connection = connector.connect().await.unwrap();
    engine
        .accept_dial_in_runtime("labpc", connection.transport)
        .await
        .expect("labpc dials in");
    let session = engine.create_session(selection).unwrap().session_id;
    engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(workspace.path().to_owned()),
            cookie_agent_protocol::EventOrigin::new("user").unwrap(),
        )
        .await
        .expect("attach labpc");
    // Idle for ten minutes: the pings keep it up.
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(labpc_listing(engine).connected);
    assert_eq!(
        instance_status(registry, "labpc"),
        crate::RuntimeInstanceStatus::Ready
    );

    connector.silence();
    let silenced = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), async {
        while labpc_listing(engine).connected {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the silent connection is ended");
    // Last heard up to a ping interval before the silence began.
    assert!(
        silenced.elapsed() <= Duration::from_secs(45),
        "{:?}",
        silenced.elapsed()
    );
    assert!(
        silenced.elapsed() >= Duration::from_secs(30),
        "{:?}",
        silenced.elapsed()
    );
    wait_for_status(registry, "labpc", crate::RuntimeInstanceStatus::Failed).await;
    assert_eq!(
        labpc_listing(engine).message.as_deref(),
        Some("runtime `labpc` disconnected: nothing arrived from it for 45 s")
    );
    fixture.engine.shutdown().await;
}

/// A runtime tool that never ends and ignores `tool/cancel`, as a wedged
/// call does.
struct WedgedTool {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolProvider for WedgedTool {
    fn provider_id(&self) -> &'static str {
        "test.wedged"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            background: false,
            background_default: false,
            name: "wedge".into(),
            collision: Default::default(),
            permission_name: None,
            description: "Never finish.".into(),
            parameters: authoring::schema::<NoArgs>(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            output: Default::default(),
        }])
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Ok(None)
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((None, None))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("wedge".into())
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let action = cookie_agent_protocol::PermissionAction::new("wedge")
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let resource = authoring::prepared_resource(
            action.clone(),
            "wedge",
            b"wedge",
            b"wedge",
            cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
            cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = authoring::prepared_operation(
            "wedge",
            &call.arguments,
            vec![(action, "wedge")],
            vec![resource],
            &ctx.context_bytes(),
        )?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(WedgedExecutor(Arc::clone(&self.started))),
        )
    }
}

struct WedgedExecutor(Arc<tokio::sync::Notify>);

#[async_trait]
impl PreparedExecutor for WedgedExecutor {
    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        self.0.notify_one();
        tokio::time::sleep(Duration::from_secs(600)).await;
        Err(ToolError::execution("woke up"))
    }
}

/// A session working only on `remote` whose run's first model turn calls
/// `tool` with `arguments`, and the run.
async fn remote_tool_run(
    connector: &Arc<TestConnector>,
    permissions: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> (
    crate::runtime::tests::support::Fixture,
    crate::runtime::tests::support::PanicResistantTempDir,
    SessionId,
    RunId,
) {
    use crate::runtime::tests::support::{
        MatchedScriptedResponse, custom_fixture_with_endpoint_and_primary_agent,
        scripted_channel_server, scripted_tool_body,
    };
    let (endpoint, responses, _server) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body("remote-call", tool, arguments),
        ))
        .unwrap();
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        &format!(
            "---\ndescription: Remote interrupt agent\nmode: primary\nenabled: true\nmodels: [{{ model: \"custom.test/group/model\", variant: base }}]\npermissions:\n{permissions}\n---\nInterrupt test.\n"
        ),
    );
    register_remote(&fixture, connector);
    let workspace = private_tempdir();
    let session = fixture
        .engine
        .create_session_with_permissions(
            selection.clone(),
            None,
            Vec::new(),
            crate::SessionRuntimes {
                runtimes: Some(vec![crate::SessionRuntimeRequest {
                    runtime_id: "remote".into(),
                    capability: None,
                    workspace: Some(workspace.path().to_owned()),
                }]),
                global: true,
                cwd: None,
            },
        )
        .await
        .unwrap()
        .session_id;
    let run = fixture
        .engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("remote-interrupt").unwrap(),
                selection,
                content: "start the call".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap()
        .run_id;
    (fixture, workspace, session, run)
}

/// Cancels `run` and returns how long its terminal event took, with the
/// termination of its one call.
async fn interrupt_remote_run(
    engine: &crate::Engine,
    session: SessionId,
    run: RunId,
) -> (Duration, cookie_agent_protocol::ToolCallTermination) {
    let started = std::time::Instant::now();
    engine
        .cancel_run(
            run,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    crate::runtime::tests::support::await_event(engine, session, "the run's end", |event| {
        event.run_id == Some(run)
            && matches!(
                event.payload,
                cookie_agent_protocol::EventPayload::RunFinished { .. }
            )
    })
    .await;
    let elapsed = started.elapsed();
    let termination = engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::ToolCallTerminated { termination } => {
                Some(termination.clone())
            }
            _ => None,
        })
        .expect("the call terminated");
    (elapsed, termination)
}

/// Interrupting a run stops a remote runtime's bash call at once: the
/// daemon sends `tool/cancel` and the runtime kills the command.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn interrupting_a_run_stops_a_remote_bash_at_once() {
    let markers = private_tempdir();
    let ready = markers.path().join("ready");
    let connector = TestConnector::builtin();
    let (fixture, _workspace, session, run) = remote_tool_run(
        &connector,
        "  bash: allow",
        "bash",
        serde_json::json!({"command": format!("touch '{}'; sleep 60", ready.display())}),
    )
    .await;
    tokio::time::timeout(WAIT, async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the command started");
    let (elapsed, termination) = interrupt_remote_run(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: remote bash `sleep 60`: {elapsed:?}");
    assert_eq!(
        termination.outcome,
        cookie_agent_protocol::ToolTerminationOutcome::Cancelled
    );
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    fixture.engine.shutdown().await;
}

/// A remote call that ignores `tool/cancel` ends as cancelled once the
/// cleanup grace runs out; the run's terminal event does not wait for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_wedged_remote_call_does_not_hold_the_run_s_cancellation() {
    let started = Arc::new(tokio::sync::Notify::new());
    let tool_started = Arc::clone(&started);
    let connector = TestConnector::new(Arc::new(move |_| {
        vec![Arc::new(WedgedTool {
            started: Arc::clone(&tool_started),
        })]
    }));
    let (fixture, _workspace, session, run) =
        remote_tool_run(&connector, "  {}", "wedge", serde_json::json!({})).await;
    tokio::time::timeout(WAIT, started.notified())
        .await
        .expect("the call runs");
    let (elapsed, termination) = interrupt_remote_run(&fixture.engine, session, run).await;
    eprintln!("interrupt timing: wedged remote call: {elapsed:?}");
    assert_eq!(
        termination.outcome,
        cookie_agent_protocol::ToolTerminationOutcome::Cancelled
    );
    assert!(
        termination
            .error
            .as_ref()
            .is_some_and(|error| error.message.as_str().contains("cleanup deadline elapsed")),
        "{termination:?}"
    );
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    fixture.engine.shutdown().await;
}

/// The runtime drops a call that ignores `tool/cancel` once the grace runs
/// out, so its execute reply still comes.
#[tokio::test]
async fn the_runtime_drops_a_cancelled_call_that_does_not_stop() {
    let workspace = private_tempdir();
    let started = Arc::new(tokio::sync::Notify::new());
    let tool_started = Arc::clone(&started);
    let connector = TestConnector::new(Arc::new(move |_| {
        vec![Arc::new(WedgedTool {
            started: Arc::clone(&tool_started),
        })]
    }));
    let (runtime, provider) = connect(&connector, workspace.path()).await;
    let prepared = prepare(&provider, &runtime, "wedge", serde_json::json!({})).await;
    let cancellation = CancellationToken::new();
    let call = call_context(cancellation.clone());
    let execution = tokio::spawn(execute(prepared, call.context));
    tokio::time::timeout(WAIT, started.notified())
        .await
        .expect("the call runs");
    let cancelled = std::time::Instant::now();
    cancellation.cancel();
    let result = tokio::time::timeout(WAIT, execution)
        .await
        .expect("the call ends")
        .unwrap();
    assert!(cancelled.elapsed() < Duration::from_secs(4));
    let error = result.expect_err("a dropped call fails");
    assert!(
        error.to_string().contains("did not stop within 2 s"),
        "{error}"
    );
}

/// The stdio fixture server as `[runtimes.local.mcp]` or `[mcp]` lists it,
/// with `env`.
fn fixture_server(
    env: &[(&str, &std::path::Path)],
    scope: Option<cookie_agent_config::McpServerScope>,
) -> cookie_agent_config::McpServerConfig {
    cookie_agent_config::McpServerConfig {
        command: Some(crate::runtime::tests::support::python_command().into()),
        args: vec![
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../mcp/tests/fixtures/mcp_server.py"
            )
            .into(),
        ],
        env: env
            .iter()
            .map(|(name, path)| ((*name).to_owned(), path.to_string_lossy().into_owned()))
            .collect(),
        cwd: None,
        url: None,
        headers: std::collections::BTreeMap::new(),
        oauth: cookie_agent_config::McpOAuthConfig::default(),
        enabled: true,
        timeout_ms: Some(30_000),
        scope,
    }
}

/// An engine whose `local` runtime hosts `servers` and whose global
/// runtime hosts `global`.
async fn open_with_servers(
    fixture: &crate::runtime::tests::support::Fixture,
    servers: Vec<(&str, cookie_agent_config::McpServerConfig)>,
    global: Vec<(&str, cookie_agent_config::McpServerConfig)>,
) -> crate::Engine {
    let directory = fixture._directory.path();
    let mut config = fixture.config.clone();
    config.runtimes.insert(
        cookie_agent_identity::RuntimeId::new("local").unwrap(),
        cookie_agent_config::RuntimeTemplateConfig {
            command: Vec::new(),
            env: std::collections::BTreeMap::new(),
            startup_timeout_ms: 10_000,
            shutdown_grace_ms: 3_000,
            mcp_servers: servers
                .into_iter()
                .map(|(name, server)| (name.to_owned(), server))
                .collect(),
        },
    );
    for (name, server) in global {
        config.mcp_servers.insert(
            name.to_owned(),
            cookie_agent_config::LoadedMcpServer {
                source: cookie_agent_config::McpServerSource::UserFile,
                config: server,
            },
        );
    }
    crate::Engine::open_async(crate::EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.to_owned(),
        config,
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
    .expect("engine with MCP servers")
}

fn listed(
    engine: &crate::Engine,
    runtime: &str,
) -> Vec<(
    String,
    Option<String>,
    Option<cookie_agent_protocol::McpServerScope>,
)> {
    let mut servers = engine
        .list_mcp_servers()
        .into_iter()
        .filter(|server| server.runtime_id.as_str() == runtime)
        .map(|server| (server.name, server.workspace, server.scope))
        .collect::<Vec<_>>();
    servers.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    servers
}

/// A runtime-scoped server of the `local` runtime: listed once, without a
/// workspace, its tools composed once per session under the runtime's ID
/// however many of the runtime's workspaces the session attaches, called
/// without a workspace, and reconnected through the runtime.
#[tokio::test]
async fn a_runtime_scoped_server_is_listed_and_composed_once_per_runtime() {
    use cookie_agent_protocol::McpServerScope as Scope;
    let (fixture, selection) = custom_fixture();
    fixture.engine.shutdown().await;
    let pid_file = fixture._directory.path().join("browser.pid");
    let engine = open_with_servers(
        &fixture,
        vec![
            (
                "browser",
                fixture_server(
                    &[("MCP_FIXTURE_PID_FILE", &pid_file)],
                    Some(cookie_agent_config::McpServerScope::Runtime),
                ),
            ),
            ("docs", fixture_server(&[], None)),
        ],
        Vec::new(),
    )
    .await;
    let local = engine
        .inner
        .runtimes
        .local_workspace_root()
        .to_string_lossy()
        .into_owned();
    // The runtime-scoped server once, the workspace-scoped one where the
    // local instance hosts it.
    assert_eq!(
        listed(&engine, "local"),
        [
            ("browser".to_owned(), None, Some(Scope::Runtime)),
            (
                "docs".to_owned(),
                Some(local.clone()),
                Some(Scope::Workspace)
            ),
        ]
    );
    let browser = engine
        .list_mcp_servers()
        .into_iter()
        .find(|server| server.name == "browser")
        .unwrap();
    assert_eq!(
        browser.state,
        cookie_agent_protocol::McpServerState::Connected
    );
    assert_eq!(browser.tool_count, 2);

    // A session with two workspaces of `local` gets its tools once.
    let session = engine.create_session(selection).unwrap().session_id;
    let second = private_tempdir();
    engine
        .attach_runtime(
            session,
            "local",
            None,
            Some(second.path().to_owned()),
            cookie_agent_protocol::EventOrigin::new("user").unwrap(),
        )
        .await
        .unwrap();
    let composition = engine.compose_session_tools(session, |_, _| true).unwrap();
    let names = composition
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names
            .iter()
            .filter(|name| name.ends_with("browser_echo_text"))
            .collect::<Vec<_>>(),
        [&"browser_echo_text"]
    );
    // The workspace-scoped one is offered by each workspace, so prefixed.
    assert_eq!(
        names
            .iter()
            .filter(|name| name.ends_with("docs_echo_text"))
            .count(),
        2,
        "{names:?}"
    );
    let echo = composition.get("browser_echo_text").unwrap();
    let runtime = echo.source.origin.runtime().expect("a runtime tool");
    assert_eq!(runtime.id(), "local");
    assert!(runtime.workspace().is_none(), "it runs without a workspace");
    assert_eq!(
        echo.source.provider.tool_source(&echo.spec.name),
        Some(cookie_agent_runtime_api::ProvidedToolSource::Mcp {
            server: "browser".into()
        })
    );
    let pid = std::fs::read_to_string(&pid_file).unwrap();

    // Called through the session's composition.
    let prepared = echo
        .source
        .provider
        .prepare(
            ToolPreparationContext {
                session,
                run: RunId::new_v7(),
                origin: echo.source.origin.clone(),
                turn_context: turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: echo.spec.name.clone(),
                arguments: serde_json::json!({"text": "shared"}),
            },
        )
        .await
        .expect("prepared");
    let call = call_context(CancellationToken::new());
    let completion = execute(prepared, call.context).await.expect("executed");
    assert!(!completion.failed);
    assert_eq!(
        completion.output,
        cookie_agent_protocol::ToolCompletionOutput::Single {
            text: "shared".into()
        }
    );

    // Reconnecting it goes to the runtime, not a workspace.
    let reconnected = engine
        .reconnect_runtime_mcp_server("local", None, "browser".into())
        .await
        .expect("reconnected")
        .expect("listed");
    assert_eq!(reconnected.scope, Some(Scope::Runtime));
    assert_ne!(
        std::fs::read_to_string(&pid_file).unwrap(),
        pid,
        "restarted"
    );
    let error = engine
        .reconnect_runtime_mcp_server("local", None, "docs".into())
        .await
        .expect_err("a workspace-scoped server reconnects in its workspace");
    assert!(
        error.to_string().contains("starts when a workspace opens"),
        "{error}"
    );
    engine.shutdown().await;
}

/// A dial-in runtime with no workspace open lists its configured servers:
/// the runtime-scoped one live, the workspace-scoped one as starting with
/// a workspace.
#[tokio::test]
async fn a_connected_runtime_lists_its_servers_before_any_workspace_opens() {
    use cookie_agent_protocol::McpServerScope as Scope;
    let (fixture, _) = custom_fixture();
    let template = fixture
        .engine
        .inner
        .runtimes
        .dial_in_template("labpc")
        .unwrap();
    let (daemon, runtime) = in_process_pair(64);
    let config =
        |server: cookie_agent_config::McpServerConfig| serde_json::to_value(server).unwrap();
    tokio::spawn(serve(
        Box::new(runtime),
        ServeOptions {
            mcp_servers: vec![
                cookie_agent_runtime_api::protocol::McpServerSettings {
                    name: "browser".into(),
                    config: config(fixture_server(
                        &[],
                        Some(cookie_agent_config::McpServerScope::Runtime),
                    )),
                },
                cookie_agent_runtime_api::protocol::McpServerSettings {
                    name: "lsp".into(),
                    config: config(fixture_server(&[], None)),
                },
            ],
            ..ServeOptions::default()
        },
    ));
    let settings = runtime_settings(&fixture.config, "labpc");
    template
        .connect(
            cookie_agent_runtime_api::RuntimeConnection::new(Box::new(daemon)),
            &settings,
            Some(Arc::clone(&fixture.engine.inner.mcp_hub)),
        )
        .await
        .expect("dialed in");
    let deadline = tokio::time::Instant::now() + WAIT;
    let servers = loop {
        let servers = fixture
            .engine
            .list_mcp_servers()
            .into_iter()
            .filter(|server| server.runtime_id.as_str() == "labpc")
            .collect::<Vec<_>>();
        if servers.iter().any(|server| {
            server.name == "browser"
                && server.state == cookie_agent_protocol::McpServerState::Connected
        }) {
            break servers;
        }
        assert!(tokio::time::Instant::now() < deadline, "{servers:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let summary = servers
        .iter()
        .map(|server| {
            (
                server.name.as_str(),
                server.workspace.as_deref(),
                server.scope,
                server.source,
                server.state,
                server.tool_count,
                server.message.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (
                "browser",
                None,
                Some(Scope::Runtime),
                cookie_agent_protocol::McpConfigSource::Machine,
                cookie_agent_protocol::McpServerState::Connected,
                2,
                None
            ),
            (
                "lsp",
                None,
                Some(Scope::Workspace),
                cookie_agent_protocol::McpConfigSource::Machine,
                cookie_agent_protocol::McpServerState::Disconnected,
                0,
                Some("starts when a workspace opens")
            ),
        ]
    );
    fixture.engine.shutdown().await;
}

/// A top-level server without a `cwd` runs in the daemon user's home, not
/// wherever the daemon started.
#[tokio::test]
async fn a_global_server_runs_in_the_daemon_users_home() {
    let (fixture, _) = custom_fixture();
    fixture.engine.shutdown().await;
    let cwd_file = fixture._directory.path().join("global.cwd");
    let engine = open_with_servers(
        &fixture,
        Vec::new(),
        vec![(
            "docs",
            fixture_server(&[("MCP_FIXTURE_CWD_FILE", &cwd_file)], None),
        )],
    )
    .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cwd_file.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = std::path::PathBuf::from(std::fs::read_to_string(&cwd_file).unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(
        Some(started.to_string_lossy().into_owned()),
        crate::runtimes::daemon_home()
    );
    engine.shutdown().await;
}
