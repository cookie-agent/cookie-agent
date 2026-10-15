//! The daemon's end of the runtime protocol: runtime instances that run in
//! another task or process and are reached over a frame transport.
//!
//! A [`ProtocolTemplate`] starts an instance with its [`RuntimeConnector`]
//! and sends `runtime/initialize`. The [`RuntimeClient`] it gets back keeps
//! the connection: its [`RemoteToolProvider`] publishes the tools the runtime
//! declared and proxies preparation and execution, it routes `tool/progress`
//! into the call's capture, and it maps `job/*` onto the engine's background
//! jobs. When the connection is lost, calls in flight fail with
//! `runtime_unavailable` and running background jobs complete as failed.
//!
//! A runtime in a child process plugs in as another [`RuntimeConnector`].

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use base64::Engine as _;
use cookie_agent_config::{SkillDiscovery, SkillRegistry};
use cookie_agent_protocol::{SessionId, ToolCallId, ToolCallPresentation};
use cookie_agent_runtime_api::{
    BackgroundJob, BackgroundJobs, BackgroundReport, PreparedDeclaration, PreparedExecutor,
    PreparedSerializationKey, PreparedTool, ProgressSink, PromptSection, RuntimeConnector,
    RuntimeInfo, SessionToolContext, ToolArtifacts, ToolCall, ToolCompletion, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProvider, ToolSpec, ToolStdin,
    WorkspaceContext,
    context::AgentMdContext,
    protocol::{
        self, ContextParams, ContextResult, Empty, ExecuteOutcome, ExecuteParams, ExecuteResult,
        InitializeParams, InitializeResult, InlineMedia, JobCompletedParams, JobProgressParams,
        JobRef, JobStartedParams, Outcome, PrepareOutcome, PrepareParams, PrepareResult,
        PreparedCall, PreparedRef, RUNTIME_PROTOCOL_VERSION, RuntimeSettings, RuntimeSkills,
        RuntimeTools, StdinParams, ToolProgressParams, WireBackgroundReport, WireToolCall,
        error_code, method,
    },
    rpc::{Incoming, IncomingNotification, IncomingRequest, Peer, RpcError},
};
use tokio::sync::oneshot;

use super::registry::{RuntimeTemplate, StartedRuntime};
use crate::EngineError;

/// Failed preparations whose presentation the engine has not asked for yet.
const MAX_PENDING_PRESENTATIONS: usize = 256;

/// A template whose instances speak the runtime protocol.
pub(crate) struct ProtocolTemplate {
    runtime_id: String,
    takes_workspace: bool,
    /// The workspace an instance gets when the template takes none, such as
    /// the daemon's working directory for `local`.
    fixed_workspace: Option<PathBuf>,
    connector: Arc<dyn RuntimeConnector>,
    settings: RuntimeSettings,
}

impl ProtocolTemplate {
    /// The `local` coding runtime for `workspace`.
    pub(crate) fn local(
        connector: Arc<dyn RuntimeConnector>,
        workspace: &Path,
        settings: RuntimeSettings,
    ) -> Self {
        Self {
            runtime_id: cookie_agent_runtime_api::LOCAL_RUNTIME_ID.to_owned(),
            takes_workspace: false,
            fixed_workspace: Some(workspace.to_owned()),
            connector,
            settings,
        }
    }
}

#[async_trait]
impl RuntimeTemplate for ProtocolTemplate {
    fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    fn takes_workspace(&self) -> bool {
        self.takes_workspace
    }

    async fn start(&self, workspace: Option<&Path>) -> Result<StartedRuntime, String> {
        let transport = self.connector.connect().await?;
        let workspace = workspace.or(self.fixed_workspace.as_deref());
        let (client, result) =
            RuntimeClient::initialize(transport, &self.runtime_id, workspace, &self.settings)
                .await?;
        started(client, result)
    }
}

/// The settings a runtime gets from the daemon's configuration.
pub(crate) fn runtime_settings(
    config: &cookie_agent_config::LoadedConfiguration,
) -> RuntimeSettings {
    let limits = crate::preview::PreviewLimits::from(&config.runtime.tool_output);
    RuntimeSettings {
        tool_output: protocol::ToolOutputLimits {
            max_lines: limits.max_lines as u64,
            max_bytes: limits.max_bytes as u64,
            head_lines: limits.head_lines as u64,
            tail_lines: limits.tail_lines as u64,
        },
        agent_md: protocol::AgentMdLimits {
            max_bytes: cookie_agent_runtime_api::context::AGENT_MD_MAX_BYTES,
        },
        mcp_servers: Vec::new(),
    }
}

/// One connection to a runtime instance. Dropping the last handle closes it.
#[derive(Clone)]
pub(crate) struct RuntimeClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    peer: Peer,
    runtime_id: String,
    tools: Mutex<Arc<DeclaredTools>>,
    /// Executing calls by prepared ID: the host services their progress and
    /// background jobs reach.
    executions: Mutex<HashMap<u64, ExecutionHost>>,
    /// Background jobs by the runtime's job ID.
    jobs: Mutex<HashMap<u64, DaemonJob>>,
    /// Presentations of calls that failed to prepare, by call.
    presentations: Mutex<VecDeque<(ToolCallId, ToolCallPresentation)>>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[derive(Default)]
struct DeclaredTools {
    specs: Vec<ToolSpec>,
    prompt_sections: Vec<PromptSection>,
}

impl From<RuntimeTools> for DeclaredTools {
    fn from(tools: RuntimeTools) -> Self {
        Self {
            specs: tools
                .tools
                .into_iter()
                .map(protocol::ToolDeclaration::into_spec)
                .collect(),
            prompt_sections: tools
                .prompt_sections
                .into_iter()
                .map(PromptSection::from)
                .collect(),
        }
    }
}

struct ExecutionHost {
    session: SessionId,
    progress: ProgressSink,
    background: Option<Arc<dyn BackgroundJobs>>,
}

struct DaemonJob {
    /// The prepared ID of the call that reserved it.
    execution: u64,
    progress: ProgressSink,
    /// The engine's reservation, until the call's reply says whether the
    /// runtime launched the job.
    reserved: Option<BackgroundJob>,
    report: Option<oneshot::Receiver<WireBackgroundReport>>,
    completion: Option<oneshot::Sender<WireBackgroundReport>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl RuntimeClient {
    /// Connects over `transport` and initializes the runtime. The error is
    /// a human-readable reason.
    pub(crate) async fn initialize(
        transport: Box<dyn cookie_agent_protocol::Transport>,
        runtime_id: &str,
        workspace: Option<&Path>,
        settings: &RuntimeSettings,
    ) -> Result<(Self, InitializeResult), String> {
        let (peer, incoming) = Peer::start(transport);
        let inner = Arc::new(ClientInner {
            peer,
            runtime_id: runtime_id.to_owned(),
            tools: Mutex::default(),
            executions: Mutex::default(),
            jobs: Mutex::default(),
            presentations: Mutex::default(),
        });
        tokio::spawn(handle_incoming(Arc::downgrade(&inner), incoming));
        let result: InitializeResult = inner
            .peer
            .request(
                method::INITIALIZE,
                &InitializeParams {
                    version: RUNTIME_PROTOCOL_VERSION,
                    runtime_id: runtime_id.to_owned(),
                    workspace: workspace.map(|path| path.to_string_lossy().into_owned()),
                    settings: settings.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        if result.version != RUNTIME_PROTOCOL_VERSION {
            return Err(format!(
                "runtime protocol version mismatch: the daemon speaks version \
                 {RUNTIME_PROTOCOL_VERSION}, the runtime speaks version {}",
                result.version
            ));
        }
        *lock(&inner.tools) = Arc::new(result.tools.clone().into());
        Ok((Self { inner }, result))
    }

    /// Whether the connection has ended.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.peer.is_closed()
    }

    /// Waits for the connection to end and returns why it did.
    pub(crate) fn closed(&self) -> impl std::future::Future<Output = String> + Send + 'static {
        let peer = self.inner.peer.clone();
        async move { peer.closed().await }
    }

    /// Asks the runtime to stop and waits up to `grace` for its answer.
    pub(crate) async fn shutdown(&self, grace: std::time::Duration) {
        let _ = tokio::time::timeout(
            grace,
            self.inner
                .peer
                .request::<_, Empty>(method::SHUTDOWN, &Empty {}),
        )
        .await;
    }

    /// The runtime's `AGENTS.md` context for a run of `preset`.
    pub(crate) async fn agent_md(
        &self,
        preset: Option<&str>,
    ) -> Result<Option<AgentMdContext>, EngineError> {
        let reply: Result<ContextResult, RpcError> = self
            .inner
            .peer
            .request(
                method::CONTEXT,
                &ContextParams {
                    preset: preset.map(str::to_owned),
                },
            )
            .await;
        match reply {
            Ok(context) => Ok(Some(AgentMdContext {
                entries: context.agent_md,
                skipped: context.skipped,
            })),
            // A runtime that is gone publishes nothing, its context included.
            Err(RpcError::Closed(_)) => Ok(None),
            Err(RpcError::Remote {
                code: error_code::CONTEXT_FAILED,
                message,
                data,
            }) => match data
                .as_ref()
                .and_then(|data| data.get("path"))
                .and_then(serde_json::Value::as_str)
            {
                Some(path) => Err(EngineError::AgentMdIo {
                    path: PathBuf::from(path),
                    source: std::io::Error::other(message),
                }),
                None => Err(EngineError::Runtime(message)),
            },
            Err(error) => Err(EngineError::Runtime(format!(
                "runtime `{}` context: {error}",
                self.inner.runtime_id
            ))),
        }
    }
}

/// The instance an initialize reply describes.
fn started(client: RuntimeClient, result: InitializeResult) -> Result<StartedRuntime, String> {
    {
        let workspace = match (result.kind, result.workspace) {
            (cookie_agent_protocol::RuntimeKind::Coding, Some(root)) => Some(
                WorkspaceContext::reported(PathBuf::from(root), result.platform),
            ),
            (cookie_agent_protocol::RuntimeKind::Coding, None) => {
                return Err("a coding runtime reported no workspace".into());
            }
            (_, _) => None,
        };
        let attachment_name = cookie_agent_protocol::AttachmentName::new(
            result.attachment_name.as_str(),
        )
        .map_err(|_| {
            format!(
                "the runtime reported invalid attachment name `{}`",
                result.attachment_name
            )
        })?;
        let skills = skill_registry(result.skills)?;
        let info = RuntimeInfo::new(
            &client.inner.runtime_id,
            result.kind,
            attachment_name.as_str(),
            workspace,
        );
        Ok(StartedRuntime {
            info,
            providers: vec![Arc::new(RemoteToolProvider {
                client: client.clone(),
            })],
            skills,
            connection: Some(client),
        })
    }
}

/// The skills a runtime reported, parsed with the daemon's rules.
fn skill_registry(skills: RuntimeSkills) -> Result<SkillRegistry, String> {
    let documents = skills
        .skills
        .into_iter()
        .map(|skill| {
            cookie_agent_config::parse_skill_document(
                &skill.document,
                skill.source,
                Path::new(&skill.path),
                Path::new(&skill.base_dir),
                skill
                    .supporting_files
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("the runtime reported an invalid skill: {error}"))?;
    let discoveries = skills
        .discoveries
        .into_iter()
        .map(|discovery| SkillDiscovery {
            name: discovery.name,
            source: discovery.source,
            path: PathBuf::from(discovery.path),
            precedence_winner: discovery.precedence_winner,
        })
        .collect();
    SkillRegistry::from_parts(documents, discoveries).map_err(|error| error.to_string())
}

async fn handle_incoming(
    client: Weak<ClientInner>,
    mut incoming: tokio::sync::mpsc::Receiver<Incoming>,
) {
    while let Some(message) = incoming.recv().await {
        let Some(client) = client.upgrade() else {
            return;
        };
        match message {
            Incoming::Request(request) => {
                tokio::spawn(async move { client.handle_request(request).await });
            }
            Incoming::Notification(notification) => client.handle_notification(&notification),
        }
    }
    if let Some(client) = client.upgrade() {
        client.connection_lost();
    }
}

impl ClientInner {
    fn unavailable(&self, error: &RpcError) -> ToolError {
        match error {
            RpcError::Closed(reason) => ToolError::runtime_unavailable(format!(
                "runtime `{}` is unavailable: {reason}",
                self.runtime_id
            )),
            other => ToolError::execution(format!("runtime `{}`: {other}", self.runtime_id)),
        }
    }

    async fn handle_request(self: Arc<Self>, request: IncomingRequest) {
        let result = match request.method.as_str() {
            method::TOOL_PROGRESS => self.call_progress(&request).await,
            method::JOB_STARTED => self.job_started(&request).await,
            method::JOB_PROGRESS => self.job_progress(&request).await,
            other => Err(RpcError::remote(
                error_code::METHOD_NOT_FOUND,
                format!("unknown method `{other}`"),
            )),
        };
        let _ = match result {
            Ok(outcome) => request.respond(&outcome).await,
            Err(error) => request.fail(error).await,
        };
    }

    fn handle_notification(&self, notification: &IncomingNotification) {
        match notification.method.as_str() {
            method::TOOLS_CHANGED => {
                if let Ok(tools) = notification.params::<RuntimeTools>() {
                    *lock(&self.tools) = Arc::new(tools.into());
                }
            }
            method::JOB_COMPLETED => {
                if let Ok(JobCompletedParams { job, report }) = notification.params()
                    && let Some(completion) = lock(&self.jobs)
                        .get_mut(&job)
                        .and_then(|job| job.completion.take())
                {
                    let _ = completion.send(report);
                }
            }
            _ => {}
        }
    }

    async fn call_progress(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let ToolProgressParams {
            prepared_id,
            progress,
        } = request.params()?;
        let sink = lock(&self.executions)
            .get(&prepared_id)
            .map(|host| host.progress.clone());
        let Some(sink) = sink else {
            return Ok(Outcome::of(Err(ToolError::ProgressSinkClosed)));
        };
        Ok(Outcome::of(sink.send(progress.into()).await))
    }

    async fn job_started(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let JobStartedParams { prepared_id, job } = request.params()?;
        let host = lock(&self.executions)
            .get(&prepared_id)
            .map(|host| (host.session, host.background.clone()));
        let Some((session, Some(background))) = host else {
            return Ok(Outcome::of(Err(ToolError::execution(
                "background jobs are unavailable here",
            ))));
        };
        let reserved = match background.reserve(session).await {
            Ok(reserved) => reserved,
            Err(error) => return Ok(Outcome::of(Err(error))),
        };
        let (completion, report) = oneshot::channel();
        lock(&self.jobs).insert(
            job,
            DaemonJob {
                execution: prepared_id,
                progress: reserved.progress(),
                reserved: Some(reserved),
                report: Some(report),
                completion: Some(completion),
            },
        );
        Ok(Outcome::default())
    }

    async fn job_progress(&self, request: &IncomingRequest) -> Result<Outcome, RpcError> {
        let JobProgressParams { job, progress } = request.params()?;
        let sink = lock(&self.jobs).get(&job).map(|job| job.progress.clone());
        let Some(sink) = sink else {
            return Ok(Outcome::of(Err(ToolError::ProgressSinkClosed)));
        };
        Ok(Outcome::of(sink.send(progress.into()).await))
    }

    /// The connection ended: calls fail through their pending replies, and
    /// running jobs complete as failed.
    fn connection_lost(&self) {
        lock(&self.executions).clear();
        lock(&self.jobs).clear();
    }

    fn remember_presentation(&self, call: ToolCallId, presentation: ToolCallPresentation) {
        let mut presentations = lock(&self.presentations);
        if presentations.len() == MAX_PENDING_PRESENTATIONS {
            presentations.pop_front();
        }
        presentations.push_back((call, presentation));
    }

    fn take_presentation(&self, call: ToolCallId) -> Option<ToolCallPresentation> {
        let mut presentations = lock(&self.presentations);
        let index = presentations.iter().position(|(id, _)| *id == call)?;
        presentations
            .remove(index)
            .map(|(_, presentation)| presentation)
    }

    async fn execute(
        self: &Arc<Self>,
        prepared_id: u64,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        let ToolExecutionContext {
            session,
            run,
            progress,
            cancellation,
            stdin,
            turn_context,
            artifacts,
            background,
        } = context;
        let params = ExecuteParams {
            prepared_id,
            session,
            run,
            turn: turn_context.as_ref().into(),
            stdin: stdin.is_some(),
            background: background.is_some(),
        };
        lock(&self.executions).insert(
            prepared_id,
            ExecutionHost {
                session,
                progress,
                background,
            },
        );
        let mut guard = ExecutionGuard {
            client: self,
            prepared_id,
            finished: false,
        };
        let request = self
            .peer
            .request::<_, ExecuteResult>(method::TOOL_EXECUTE, &params);
        tokio::pin!(request);
        let mut stdin_open = stdin.is_some();
        let forward = self.forward_stdin(prepared_id, stdin);
        tokio::pin!(forward);
        let mut cancel_sent = false;
        let reply = loop {
            tokio::select! {
                reply = &mut request => break reply,
                () = cancellation.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    let _ = self.peer.notify(method::TOOL_CANCEL, &PreparedRef { prepared_id }).await;
                }
                () = &mut forward, if stdin_open => stdin_open = false,
            }
        };
        guard.finished = true;
        drop(guard);
        let reply = reply.map_err(|error| {
            self.release_jobs(prepared_id, &[], true);
            self.unavailable(&error)
        })?;
        self.release_jobs(prepared_id, &reply.launched_jobs, false);
        let completion = match reply.outcome {
            ExecuteOutcome::Completed {
                output,
                result,
                failed,
            } => ToolCompletion {
                output,
                result,
                failed,
            },
            ExecuteOutcome::Failed { error } => return Err(error.into()),
        };
        retain_media(artifacts.as_ref(), session, reply.media, &completion)?;
        Ok(completion)
    }

    /// Forwards the call's stdin until it ends or the runtime stops taking
    /// it.
    async fn forward_stdin(&self, prepared_id: u64, stdin: Option<ToolStdin>) {
        let Some(mut stdin) = stdin else {
            return;
        };
        while let Some(write) = stdin.recv().await {
            let mut chunks = write
                .data
                .chunks(protocol::MAX_STDIN_WRITE_BYTES)
                .peekable();
            let mut sent = false;
            while let Some(chunk) = chunks.next() {
                if !self
                    .send_stdin(prepared_id, chunk, write.eof && chunks.peek().is_none())
                    .await
                {
                    return;
                }
                sent = true;
            }
            if !sent && !self.send_stdin(prepared_id, &[], write.eof).await {
                return;
            }
        }
    }

    async fn send_stdin(&self, prepared_id: u64, data: &[u8], eof: bool) -> bool {
        let outcome: Result<Outcome, RpcError> = self
            .peer
            .request(
                method::TOOL_STDIN,
                &StdinParams {
                    prepared_id,
                    data: base64::engine::general_purpose::STANDARD.encode(data),
                    eof,
                },
            )
            .await;
        matches!(outcome, Ok(Outcome { error: None }))
    }

    /// Launches the jobs the call reserved that the runtime launched, and
    /// releases the others. When the reply is unknown, the runtime may have
    /// launched them anyway, so `kill_unlaunched` kills them there.
    fn release_jobs(self: &Arc<Self>, prepared_id: u64, launched: &[u64], kill_unlaunched: bool) {
        let mut to_launch = Vec::new();
        {
            let mut jobs = lock(&self.jobs);
            let reserved = jobs
                .iter()
                .filter(|(_, job)| job.execution == prepared_id && job.reserved.is_some())
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for id in reserved {
                if launched.contains(&id) {
                    let job = jobs.get_mut(&id).expect("reserved job is listed");
                    if let (Some(reserved), Some(report)) = (job.reserved.take(), job.report.take())
                    {
                        to_launch.push((id, reserved, report));
                    }
                } else {
                    jobs.remove(&id);
                    if kill_unlaunched {
                        self.peer
                            .notify_detached(method::JOB_KILL, &JobRef { job: id });
                    }
                }
            }
        }
        for (id, reserved, report) in to_launch {
            let proxy = job_proxy(Arc::clone(self), id, report);
            if reserved.launch(proxy).is_err() {
                lock(&self.jobs).remove(&id);
                self.peer
                    .notify_detached(method::JOB_KILL, &JobRef { job: id });
            }
        }
    }
}

/// Unregisters an execution; one dropped before its reply also cancels the
/// call in the runtime.
struct ExecutionGuard<'a> {
    client: &'a Arc<ClientInner>,
    prepared_id: u64,
    finished: bool,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        lock(&self.client.executions).remove(&self.prepared_id);
        if !self.finished {
            self.client.peer.notify_detached(
                method::TOOL_CANCEL,
                &PreparedRef {
                    prepared_id: self.prepared_id,
                },
            );
            self.client.release_jobs(self.prepared_id, &[], true);
        }
    }
}

/// Stores the media a reply carried in the session's artifact store; the
/// result's attachments must all be among it.
fn retain_media(
    artifacts: &dyn ToolArtifacts,
    session: SessionId,
    media: Vec<InlineMedia>,
    completion: &ToolCompletion,
) -> Result<(), ToolError> {
    let mut total = 0_u64;
    let mut retained = Vec::with_capacity(media.len());
    for blob in media {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&blob.data)
            .map_err(|error| {
                ToolError::execution(format!("invalid media from runtime: {error}"))
            })?;
        total += bytes.len() as u64;
        if total > protocol::MAX_INLINE_MEDIA_BYTES {
            return Err(ToolError::resource_limit(format!(
                "a runtime result carries at most {} bytes of media",
                protocol::MAX_INLINE_MEDIA_BYTES
            )));
        }
        if cookie_agent_protocol::Sha256Digest::of_bytes(&bytes).as_str() != blob.sha256 {
            return Err(ToolError::execution(
                "media from the runtime does not match its digest",
            ));
        }
        let (reference, _) = artifacts.retain(session, &bytes)?;
        if reference != protocol::artifact_reference(&blob.sha256) {
            return Err(ToolError::execution(
                "the artifact store gave runtime media an unexpected reference",
            ));
        }
        retained.push(reference);
    }
    if let Some(attachment) = completion
        .result
        .all_attachments()
        .find(|attachment| !retained.contains(&attachment.reference))
    {
        return Err(ToolError::execution(format!(
            "the runtime's result references media it did not send ({})",
            attachment.reference.uri
        )));
    }
    Ok(())
}

/// The future the engine runs for a job the runtime launched: it resolves
/// with the runtime's report, or with a failure when the runtime is gone.
/// Dropping it, which is how the engine kills a job, kills the runtime's
/// job.
async fn job_proxy(
    client: Arc<ClientInner>,
    job: u64,
    report: oneshot::Receiver<WireBackgroundReport>,
) -> BackgroundReport {
    let mut guard = JobGuard {
        client: Arc::clone(&client),
        job,
        finished: false,
    };
    let report = match report.await {
        Ok(report) => report.into(),
        Err(_) => runtime_exited_report(&client.runtime_id),
    };
    guard.finished = true;
    report
}

struct JobGuard {
    client: Arc<ClientInner>,
    job: u64,
    finished: bool,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        lock(&self.client.jobs).remove(&self.job);
        if !self.finished {
            self.client
                .peer
                .notify_detached(method::JOB_KILL, &JobRef { job: self.job });
        }
    }
}

fn runtime_exited_report(runtime_id: &str) -> BackgroundReport {
    BackgroundReport {
        summary: format!("background job failed: runtime `{runtime_id}` exited"),
        element: "background_job".into(),
        attributes: vec![
            ("runtime".into(), runtime_id.to_owned()),
            ("status".into(), "failed".into()),
        ],
        body: format!("Status: failed: runtime `{runtime_id}` exited before the job finished\n"),
    }
}

/// The tools a protocol runtime declared, as one provider. The sync
/// metadata methods are answered from the declarations and from what the
/// runtime reports when it prepares a call.
pub(crate) struct RemoteToolProvider {
    client: RuntimeClient,
}

impl RemoteToolProvider {
    fn prepared_tool(&self, call: PreparedCall) -> Result<PreparedTool, ToolError> {
        let serialization_key = call
            .serialization_key
            .as_deref()
            .map(|key| {
                protocol::hex_decode(key)
                    .map(PreparedSerializationKey::new)
                    .ok_or_else(|| ToolError::execution("invalid serialization key from runtime"))
            })
            .transpose()?;
        let executor = Box::new(RemoteExecutor {
            client: Arc::clone(&self.client.inner),
            prepared_id: call.prepared_id,
            executed: false,
        });
        Ok(PreparedTool::new(
            call.operation,
            call.normalized_arguments,
            serialization_key,
            executor,
        )?
        .with_reported_policy_labels(call.policy_labels)?
        .with_declaration(PreparedDeclaration {
            permission_name: protocol::permission_name(call.permission).to_owned(),
            permission_resource: call.permission_resource,
            presentation: call.presentation,
        }))
    }
}

#[async_trait]
impl ToolProvider for RemoteToolProvider {
    fn provider_id(&self) -> &'static str {
        "runtime.protocol"
    }

    fn prompt_sections(&self, _ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        Ok(lock(&self.client.inner.tools).prompt_sections.clone())
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(lock(&self.client.inner.tools).specs.clone())
    }

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError>
    where
        Self: Sized,
    {
        Err(ToolError::execution(
            "a runtime tool's permission is declared by its runtime",
        ))
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Err(ToolError::execution(
            "a runtime tool's permission resource is reported when it is prepared",
        ))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Err(ToolError::execution(
            "a runtime tool's display argument is reported when it is prepared",
        ))
    }

    fn presentation(&self, call: &ToolCall) -> ToolCallPresentation {
        self.client
            .inner
            .take_presentation(call.id)
            .unwrap_or_else(|| cookie_agent_runtime_api::tool_title_only(&call.name))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        self.prepare_parallel(ctx, vec![call])
            .await
            .pop()
            .expect("one result per call")
    }

    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        let client = &self.client.inner;
        let mut prepared = Vec::with_capacity(calls.len());
        let turn: protocol::WireTurnContext = ctx.turn_context.as_ref().into();
        for batch in calls.chunks(protocol::MAX_PREPARE_BATCH) {
            let reply: Result<PrepareResult, RpcError> = client
                .peer
                .request(
                    method::TOOL_PREPARE,
                    &PrepareParams {
                        session: ctx.session,
                        run: ctx.run,
                        turn: turn.clone(),
                        calls: batch
                            .iter()
                            .map(|call| WireToolCall {
                                id: call.id,
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            })
                            .collect(),
                    },
                )
                .await;
            let outcomes = match reply {
                Ok(reply) if reply.results.len() == batch.len() => reply.results,
                Ok(reply) => {
                    // Release whatever the runtime prepared before failing.
                    for outcome in reply.results {
                        if let PrepareOutcome::Prepared(call) = outcome {
                            client.peer.notify_detached(
                                method::TOOL_RELEASE,
                                &PreparedRef {
                                    prepared_id: call.prepared_id,
                                },
                            );
                        }
                    }
                    prepared.extend(batch.iter().map(|_| {
                        Err(ToolError::execution(
                            "the runtime answered a different number of calls",
                        ))
                    }));
                    continue;
                }
                Err(error) => {
                    prepared.extend(batch.iter().map(|_| Err(client.unavailable(&error))));
                    continue;
                }
            };
            for (call, outcome) in batch.iter().zip(outcomes) {
                prepared.push(match outcome {
                    PrepareOutcome::Prepared(declared) => self.prepared_tool(declared),
                    PrepareOutcome::Failed {
                        error,
                        presentation,
                    } => {
                        client.remember_presentation(call.id, presentation);
                        Err(error.into())
                    }
                });
            }
        }
        prepared
    }
}

/// Executes, revalidates, or releases one call the runtime prepared.
struct RemoteExecutor {
    client: Arc<ClientInner>,
    prepared_id: u64,
    executed: bool,
}

impl Drop for RemoteExecutor {
    fn drop(&mut self) {
        if !self.executed {
            self.client.peer.notify_detached(
                method::TOOL_RELEASE,
                &PreparedRef {
                    prepared_id: self.prepared_id,
                },
            );
        }
    }
}

#[async_trait]
impl PreparedExecutor for RemoteExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        let outcome: Outcome = self
            .client
            .peer
            .request(
                method::TOOL_REVALIDATE,
                &PreparedRef {
                    prepared_id: self.prepared_id,
                },
            )
            .await
            .map_err(|error| self.client.unavailable(&error))?;
        outcome.into_result()
    }

    async fn execute(
        mut self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        self.executed = true;
        let client = Arc::clone(&self.client);
        client.execute(self.prepared_id, context).await
    }
}

#[cfg(test)]
mod tests;
