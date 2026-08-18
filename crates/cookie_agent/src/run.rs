use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    io::{self, Read as IoRead, Write as IoWrite},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, anyhow};
use clap::{ArgGroup, Args, ValueEnum};
use cookie_agent_engine::{Engine, EngineError, events::OutputMessage};
use cookie_agent_protocol::{
    AgentId, ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision, AvailableModelDescriptor,
    ClientResponseId, ClientRunId, EventPayload, EventSubscriptionMessage, ModelKey,
    ModelSelection, OutputDelta, OutputGap, OutputStream, PermissionAction, PermissionEffect,
    PermissionMode, RunId, RunSelection, RunStartParams, SessionId, SessionMeta, SessionOrigin,
    StoredEvent, ToolCallId, UsageRollup, VariantId, WildcardPattern,
};
use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;

pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_PERMISSION: i32 = 3;
pub const EXIT_CANCELLED: i32 = 4;
pub const EXIT_ENVIRONMENT: i32 = 5;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
    None,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum PermissionModeArg {
    #[default]
    #[value(alias = "auto_approve")]
    AutoApprove,
    #[value(alias = "auto_approve_n")]
    AutoApproveN,
    #[value(alias = "auto_approve_y")]
    AutoApproveY,
    Ask,
    Yolo,
}

impl From<PermissionModeArg> for PermissionMode {
    fn from(value: PermissionModeArg) -> Self {
        match value {
            PermissionModeArg::AutoApprove => Self::AutoApprove,
            PermissionModeArg::AutoApproveN => Self::AutoApproveN,
            PermissionModeArg::AutoApproveY => Self::AutoApproveY,
            PermissionModeArg::Ask => Self::Ask,
            PermissionModeArg::Yolo => Self::Yolo,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AllowedTool {
    Read,
    Write,
    Bash,
    Delegate,
    Mcp,
    Plugin,
    PluginPermission(String),
    Skill(String),
}

impl AllowedTool {
    const fn action(&self) -> PermissionAction {
        match self {
            Self::Read => PermissionAction::Read,
            Self::Write => PermissionAction::Write,
            Self::Bash => PermissionAction::Bash,
            Self::Delegate => PermissionAction::Delegate,
            Self::Mcp => PermissionAction::Mcp,
            Self::Plugin | Self::PluginPermission(_) => PermissionAction::Plugin,
            Self::Skill(_) => PermissionAction::Skill,
        }
    }

    fn resource(&self) -> &str {
        match self {
            Self::Skill(name) | Self::PluginPermission(name) => name,
            _ => "*",
        }
    }
}

impl std::str::FromStr for AllowedTool {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "bash" => Ok(Self::Bash),
            "delegate" => Ok(Self::Delegate),
            "mcp" => Ok(Self::Mcp),
            "plugin" => Ok(Self::Plugin),
            _ => value
                .strip_prefix("skill:")
                .filter(|name| !name.is_empty())
                .map(|name| Self::Skill(name.to_owned()))
                .or_else(|| {
                    value
                        .strip_prefix("plugin:")
                        .filter(|name| !name.is_empty())
                        .map(|name| Self::PluginPermission(name.to_owned()))
                })
                .ok_or_else(|| {
                    "expected read, write, bash, delegate, mcp, plugin, plugin:<name>, or skill:<name>"
                        .into()
                }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Args)]
#[command(group(
    ArgGroup::new("prompt_source")
        .required(true)
        .multiple(false)
))]
pub struct RunArgs {
    /// Prompt submitted to the selected agent. Use '-' to read standard input.
    #[arg(value_name = "PROMPT", group = "prompt_source")]
    pub positional_prompt: Option<String>,
    /// Prompt submitted to the selected agent. Use '-' to read standard input.
    #[arg(short = 'p', long, value_name = "PROMPT", group = "prompt_source")]
    pub prompt: Option<String>,
    /// Read the prompt from a file. Use '-' to read standard input.
    #[arg(short = 'f', long, value_name = "PATH", group = "prompt_source")]
    pub prompt_file: Option<PathBuf>,
    /// Root-runnable agent ID.
    #[arg(short = 'a', long)]
    pub agent: Option<AgentId>,
    /// Available provider/model selection.
    #[arg(short = 'm', long)]
    pub model: Option<ModelKey>,
    /// Named model variant, or 'base' for no variant.
    #[arg(long)]
    pub variant: Option<String>,
    /// Permission handling mode for this session.
    #[arg(long, value_enum, default_value_t)]
    pub permission_mode: PermissionModeArg,
    /// Permission action to allow. May be repeated or comma-delimited.
    #[arg(long, value_delimiter = ',')]
    pub allowed_tools: Vec<AllowedTool>,
    /// Load a user-invocable skill before running the prompt.
    #[arg(long)]
    pub skill: Option<String>,
    /// Arguments supplied to --skill.
    #[arg(long, requires = "skill")]
    pub skill_args: Option<String>,
    /// Maximum committed root model turns.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_turns: u32,
    /// Run timeout in seconds.
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: u64,
    /// Continue an existing session instead of creating one.
    #[arg(long)]
    pub resume_session: Option<SessionId>,
    /// Override the user data directory for sessions and artifacts.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Standard-output format.
    #[arg(short = 'o', long, value_enum)]
    pub output: Option<OutputMode>,
    /// Write command output to a file instead of standard output.
    #[arg(long)]
    pub output_file: Option<PathBuf>,
    /// Emit progress and, in JSON mode, tool-output records.
    #[arg(long)]
    pub verbose: bool,
    /// Alias for '--output json'.
    #[arg(long, conflicts_with = "output")]
    pub json: bool,
}

impl RunArgs {
    pub fn output_mode(&self) -> OutputMode {
        if self.json {
            OutputMode::Json
        } else {
            self.output.unwrap_or_default()
        }
    }

    pub fn validate_cli(&self) -> Result<(), &'static str> {
        if self.output_mode() == OutputMode::None && self.output_file.is_some() {
            return Err("--output-file cannot be used with '--output none'");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CancellationCause {
    Permission,
    User,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    r#type: &'static str,
    event: &'a StoredEvent,
}

#[derive(Serialize)]
struct JsonToolOutput<'a> {
    r#type: &'static str,
    call_id: ToolCallId,
    stream: OutputStream,
    byte_offset: u64,
    data: &'a str,
}

#[derive(Serialize)]
struct JsonToolOutputGap {
    r#type: &'static str,
    call_id: ToolCallId,
    stream: OutputStream,
    next_offset: u64,
}

#[derive(Serialize)]
struct JsonSummary {
    r#type: &'static str,
    session_id: SessionId,
    run_id: RunId,
    status: TerminalStatus,
    exit_code: i32,
    turns: u32,
    approval_rejections: u32,
    event_recoveries: u32,
    cancellation: Option<CancellationCause>,
    final_text: Option<String>,
    usage: UsageRollup,
}

struct TerminalOutcome {
    status: TerminalStatus,
    final_text: Option<String>,
}

struct TerminalResult {
    outcome: TerminalOutcome,
    exit_code: i32,
}

struct RunDisplay<'a> {
    args: &'a RunArgs,
    mode: OutputMode,
}

struct DriverState {
    cursor: u64,
    turns: u32,
    approval_rejections: u32,
    event_recoveries: u32,
    rejected: HashSet<cookie_agent_protocol::ApprovalId>,
    cancellation_requested: bool,
    cancellation: Option<CancellationCause>,
    tool_outputs: Vec<mpsc::Receiver<OutputMessage>>,
}

impl DriverState {
    fn new(cursor: u64) -> Self {
        Self {
            cursor,
            turns: 0,
            approval_rejections: 0,
            event_recoveries: 0,
            rejected: HashSet::new(),
            cancellation_requested: false,
            cancellation: None,
            tool_outputs: Vec::new(),
        }
    }
}

pub async fn execute(engine: &Engine, args: RunArgs) -> i32 {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    execute_with_io(engine, args, &mut stdin, &mut stdout, &mut stderr, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

pub async fn execute_with_io<F>(
    engine: &Engine,
    args: RunArgs,
    stdin: &mut dyn IoRead,
    stdout: &mut dyn IoWrite,
    stderr: &mut dyn IoWrite,
    interrupt: F,
) -> i32
where
    F: Future<Output = ()>,
{
    if let Err(error) = args.validate_cli() {
        let _ = writeln!(stderr, "cookie run: {error}");
        return EXIT_ENVIRONMENT;
    }
    let prompt = match resolve_prompt(&args, stdin) {
        Ok(prompt) => prompt,
        Err(error) => {
            let _ = writeln!(stderr, "cookie run: {error:#}");
            return EXIT_ENVIRONMENT;
        }
    };
    let mode = args.output_mode();
    let mut file = match args.output_file.as_ref() {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Some(io::BufWriter::new(file)),
            Err(error) => {
                let _ = writeln!(stderr, "cookie run: open output file: {error}");
                return EXIT_ENVIRONMENT;
            }
        },
        None => None,
    };
    let mut sink = io::sink();
    let output: &mut dyn IoWrite = if mode == OutputMode::None {
        &mut sink
    } else if let Some(file) = file.as_mut() {
        file
    } else {
        stdout
    };

    let prepared = match prepare_run(engine, &args, prompt).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = writeln!(stderr, "cookie run: {:#}", error.error);
            return error.exit_code;
        }
    };
    progress(
        &args,
        stderr,
        &format!(
            "session {} run {} started",
            prepared.session_id, prepared.run_id
        ),
    );

    let outcome = drive_run(engine, &args, mode, output, stderr, prepared, interrupt).await;
    match outcome {
        Ok((session_id, run_id, state, terminal)) => {
            let exit_code = terminal_exit(&terminal, state.cancellation);
            if let Err(error) = write_terminal(
                engine,
                output,
                mode,
                session_id,
                run_id,
                &state,
                TerminalResult {
                    outcome: terminal,
                    exit_code,
                },
            ) {
                let _ = writeln!(stderr, "cookie run: write terminal output: {error:#}");
                return EXIT_ENVIRONMENT;
            }
            progress(&args, stderr, &format!("run {run_id} finished"));
            exit_code
        }
        Err(error) => {
            let _ = writeln!(stderr, "cookie run: headless driver failed: {error:#}");
            EXIT_FAILURE
        }
    }
}

struct PreparedRun {
    session_id: SessionId,
    root_session_id: SessionId,
    run_id: RunId,
    receiver: mpsc::Receiver<EventSubscriptionMessage>,
    replay: VecDeque<StoredEvent>,
    cursor: u64,
}

struct PrepareError {
    error: anyhow::Error,
    exit_code: i32,
}

impl PrepareError {
    fn handled(reason: String) -> Self {
        Self {
            error: anyhow!(reason).context("input handled by plugin; no run started"),
            exit_code: 0,
        }
    }

    fn setup(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            exit_code: EXIT_ENVIRONMENT,
        }
    }

    fn run(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            exit_code: EXIT_FAILURE,
        }
    }
}

async fn prepare_run(
    engine: &Engine,
    args: &RunArgs,
    prompt: String,
) -> Result<PreparedRun, PrepareError> {
    let snapshot = engine
        .runtime_snapshot()
        .map_err(|error| {
            PrepareError::setup(anyhow!(error).context("load runtime snapshot for headless run"))
        })?
        .snapshot;
    let resumed = match args.resume_session {
        Some(session_id) => Some(engine.resume(session_id).await.map_err(|error| {
            PrepareError::setup(anyhow!(error).context("resume headless session"))
        })?),
        None => None,
    };
    let selection = resolve_selection(&snapshot.agents, &snapshot.models, resumed.as_ref(), args)
        .map_err(PrepareError::setup)?;
    let session = match resumed {
        Some(session) => session,
        None => engine.create_session(selection.clone()).map_err(|error| {
            PrepareError::setup(anyhow!(error).context("create headless session"))
        })?,
    };
    let root_session_id = session_root_id(&session);
    engine
        .set_permission_mode(session.session_id, args.permission_mode.into())
        .map_err(|error| {
            PrepareError::setup(anyhow!(error).context("set headless permission mode"))
        })?;
    apply_allowed_tools(engine, session.session_id, &args.allowed_tools)
        .await
        .map_err(PrepareError::setup)?;
    let prompt = args.skill.as_ref().map_or(prompt.clone(), |skill| {
        cookie_agent_protocol::encode_skill_submission_with_prompt(
            skill,
            args.skill_args.as_deref().unwrap_or_default(),
            Some(&prompt),
        )
    });
    let cursor = engine
        .get_session(session.session_id)
        .map_err(|error| PrepareError::setup(anyhow!(error).context("reload headless session")))?
        .last_event_seq;
    let (replay, receiver) = engine
        .subscribe(session.session_id, Some(cursor))
        .await
        .map_err(|error| {
            PrepareError::setup(anyhow!(error).context("subscribe to headless session events"))
        })?;
    let run_id = engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new(Uuid::now_v7().to_string())
                .expect("UUID is a valid client run ID"),
            selection,
            input: prompt,
        })
        .await
        .map_err(|error| match error {
            EngineError::InputHandled(reason) => PrepareError::handled(reason),
            error => PrepareError::run(anyhow!(error).context("start headless run")),
        })?
        .run_id;
    Ok(PreparedRun {
        session_id: session.session_id,
        root_session_id,
        run_id,
        receiver,
        replay: replay.events.into(),
        cursor,
    })
}

fn session_root_id(session: &SessionMeta) -> SessionId {
    match &session.origin {
        SessionOrigin::Root => session.session_id,
        SessionOrigin::Delegated {
            root_session_id, ..
        } => *root_session_id,
    }
}

fn resolve_prompt(args: &RunArgs, stdin: &mut dyn IoRead) -> anyhow::Result<String> {
    match (
        args.positional_prompt.as_deref(),
        args.prompt.as_deref(),
        args.prompt_file.as_deref(),
    ) {
        (Some(prompt), None, None) | (None, Some(prompt), None) => {
            if prompt == "-" {
                // Read below.
            } else {
                return Ok(prompt.to_owned());
            }
        }
        (None, None, Some(path)) if path != Path::new("-") => {
            return std::fs::read_to_string(path)
                .with_context(|| format!("read prompt file {path:?}"));
        }
        (None, None, Some(_)) => {}
        _ => return Err(anyhow!("exactly one prompt source is required")),
    }
    let mut prompt = String::new();
    stdin
        .read_to_string(&mut prompt)
        .context("read prompt from standard input")?;
    Ok(prompt)
}

fn resolve_selection(
    agents: &[cookie_agent_protocol::AgentDescriptor],
    models: &[AvailableModelDescriptor],
    resumed: Option<&SessionMeta>,
    args: &RunArgs,
) -> anyhow::Result<RunSelection> {
    let root_session = resumed.is_none_or(|session| matches!(session.origin, SessionOrigin::Root));
    if !root_session && args.agent.is_some() {
        return Err(anyhow!("--agent cannot override a delegated session"));
    }
    let base_agent = args
        .agent
        .as_ref()
        .or_else(|| resumed.map(|session| &session.creation_selection.agent));
    let agent = match base_agent {
        Some(agent_id) => agents
            .iter()
            .find(|agent| agent.id == *agent_id && (agent.runnable_as_root || !root_session)),
        None => agents
            .iter()
            .filter(|agent| agent.runnable_as_root)
            .find(|agent| agent.id.as_str() == "primary")
            .or_else(|| agents.iter().find(|agent| agent.runnable_as_root)),
    }
    .ok_or_else(|| anyhow!("selected agent is not available for this session"))?;

    let base_model = if let Some(model) = &args.model {
        let descriptor = model_descriptor(models, model)
            .ok_or_else(|| anyhow!("model `{model}` is not available"))?;
        ModelSelection {
            model: model.clone(),
            variant: descriptor.default_variant.clone(),
        }
    } else if args.agent.is_none()
        && let Some(selection) = resumed.map(|session| &session.creation_selection.model)
        && selection_is_live(models, selection)
    {
        selection.clone()
    } else {
        agent
            .resolved_fallback
            .iter()
            .find(|selection| selection_is_live(models, selection))
            .cloned()
            .or_else(|| models.first().map(default_model_selection))
            .ok_or_else(|| anyhow!("no live model is available"))?
    };
    let mut model = base_model;
    if let Some(variant) = args.variant.as_deref() {
        model.variant = if variant == "base" {
            None
        } else {
            Some(variant.parse::<VariantId>().context("parse --variant")?)
        };
    }
    if !selection_is_live(models, &model) {
        return Err(anyhow!("selected model and variant are not available"));
    }
    Ok(RunSelection {
        agent: agent.id.clone(),
        model,
    })
}

fn model_descriptor<'a>(
    models: &'a [AvailableModelDescriptor],
    key: &ModelKey,
) -> Option<&'a AvailableModelDescriptor> {
    models.iter().find(|model| &model.key == key)
}

fn default_model_selection(descriptor: &AvailableModelDescriptor) -> ModelSelection {
    ModelSelection {
        model: descriptor.key.clone(),
        variant: descriptor.default_variant.clone(),
    }
}

fn variant_is_valid(descriptor: &AvailableModelDescriptor, variant: Option<&VariantId>) -> bool {
    variant.is_none_or(|variant| {
        descriptor
            .variants
            .iter()
            .any(|candidate| candidate.id == *variant)
    })
}

fn selection_is_live(models: &[AvailableModelDescriptor], selection: &ModelSelection) -> bool {
    model_descriptor(models, &selection.model)
        .is_some_and(|descriptor| variant_is_valid(descriptor, selection.variant.as_ref()))
}

async fn apply_allowed_tools(
    engine: &Engine,
    session_id: SessionId,
    allowed: &[AllowedTool],
) -> anyhow::Result<()> {
    let rules = allowed
        .iter()
        .map(|allowed| (allowed.action(), allowed.resource().to_owned()))
        .collect::<HashSet<_>>();
    for (action, resource) in rules {
        engine
            .set_session_permission(
                session_id,
                action,
                WildcardPattern::new(resource).context("parse allowed-tools resource")?,
                PermissionEffect::Allow,
            )
            .await
            .context("apply headless allowed-tools policy")?;
    }
    Ok(())
}

async fn drive_run<F>(
    engine: &Engine,
    args: &RunArgs,
    mode: OutputMode,
    output: &mut dyn IoWrite,
    stderr: &mut dyn IoWrite,
    mut prepared: PreparedRun,
    interrupt: F,
) -> anyhow::Result<(SessionId, RunId, DriverState, TerminalOutcome)>
where
    F: Future<Output = ()>,
{
    let mut state = DriverState::new(prepared.cursor);
    let mut approvals = tokio::time::interval(Duration::from_millis(25));
    approvals.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let timeout = tokio::time::sleep(Duration::from_secs(args.timeout));
    tokio::pin!(timeout);
    tokio::pin!(interrupt);
    let display = RunDisplay { args, mode };

    loop {
        drain_tool_output(&mut state, mode, args.verbose, output)?;
        if let Some(event) = prepared.replay.pop_front() {
            match accept_event(&mut state, event) {
                AcceptedEvent::Duplicate => continue,
                AcceptedEvent::Event(event) => {
                    if let Some(terminal) = process_event(
                        engine,
                        &display,
                        output,
                        stderr,
                        prepared.run_id,
                        &mut state,
                        *event,
                    )
                    .await?
                    {
                        drain_tool_output(&mut state, mode, args.verbose, output)?;
                        return Ok((prepared.session_id, prepared.run_id, state, terminal));
                    }
                    continue;
                }
            }
        }

        tokio::select! {
            message = prepared.receiver.recv() => {
                match message {
                    Some(EventSubscriptionMessage::Event { event }) => {
                        prepared.replay.push_back(*event);
                    }
                    Some(EventSubscriptionMessage::Gap { .. }) | None => {
                        state.event_recoveries = state.event_recoveries.saturating_add(1);
                        recover_events(engine, &mut prepared, state.cursor).await?;
                    }
                }
            }
            _ = approvals.tick(), if !state.cancellation_requested => {
                if reject_escalated(engine, prepared.root_session_id, &mut state).await? {
                    progress(args, stderr, "escalated approval rejected; cancelling run");
                    request_cancel(
                        engine,
                        prepared.run_id,
                        CancellationCause::Permission,
                        &mut state,
                    ).await?;
                }
            }
            () = &mut timeout, if !state.cancellation_requested => {
                progress(args, stderr, "timeout reached; cancelling run");
                request_cancel(
                    engine,
                    prepared.run_id,
                    CancellationCause::User,
                    &mut state,
                ).await?;
            }
            () = &mut interrupt, if !state.cancellation_requested => {
                progress(args, stderr, "interrupt received; cancelling run");
                request_cancel(
                    engine,
                    prepared.run_id,
                    CancellationCause::User,
                    &mut state,
                ).await?;
            }
        }
    }
}

enum AcceptedEvent {
    Duplicate,
    Event(Box<StoredEvent>),
}

fn accept_event(state: &mut DriverState, event: StoredEvent) -> AcceptedEvent {
    if event.seq <= state.cursor {
        AcceptedEvent::Duplicate
    } else {
        state.cursor = event.seq;
        AcceptedEvent::Event(Box::new(event))
    }
}

async fn recover_events(
    engine: &Engine,
    prepared: &mut PreparedRun,
    cursor: u64,
) -> anyhow::Result<()> {
    let (replay, receiver) = engine
        .subscribe(prepared.session_id, Some(cursor))
        .await
        .context("recover headless event subscription")?;
    prepared.receiver = receiver;
    prepared.replay = replay.events.into();
    Ok(())
}

async fn process_event(
    engine: &Engine,
    display: &RunDisplay<'_>,
    output: &mut dyn IoWrite,
    stderr: &mut dyn IoWrite,
    active_run_id: RunId,
    state: &mut DriverState,
    event: StoredEvent,
) -> anyhow::Result<Option<TerminalOutcome>> {
    if event.run_id != Some(active_run_id) {
        return Ok(None);
    }
    if display.mode == OutputMode::Json {
        write_json_line(
            output,
            &JsonEvent {
                r#type: "event",
                event: &event,
            },
        )?;
    }
    match event.payload {
        EventPayload::ToolCallStarted { start }
            if display.args.verbose && display.mode == OutputMode::Json =>
        {
            subscribe_tool_output(engine, start.tool_call_id, state, output)?;
        }
        EventPayload::ModelTurnCommitted { .. } => {
            state.turns = state.turns.saturating_add(1);
            if state.turns >= display.args.max_turns && !state.cancellation_requested {
                progress(display.args, stderr, "turn limit reached; cancelling run");
                request_cancel(engine, active_run_id, CancellationCause::User, state).await?;
            }
        }
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.reason_code
                == cookie_agent_protocol::ApprovalReasonCode::AutoApproveNRejected
                && !state.cancellation_requested =>
        {
            state.approval_rejections = state.approval_rejections.saturating_add(1);
            progress(
                display.args,
                stderr,
                "auto-approve(N) rejection; cancelling run",
            );
            request_cancel(engine, active_run_id, CancellationCause::Permission, state).await?;
        }
        EventPayload::RunCompleted { final_text } => {
            return Ok(Some(TerminalOutcome {
                status: TerminalStatus::Completed,
                final_text,
            }));
        }
        EventPayload::RunFailed { .. } => {
            return Ok(Some(TerminalOutcome {
                status: TerminalStatus::Failed,
                final_text: None,
            }));
        }
        EventPayload::RunCancelled { .. } => {
            return Ok(Some(TerminalOutcome {
                status: TerminalStatus::Cancelled,
                final_text: None,
            }));
        }
        EventPayload::RunInterrupted { .. } => {
            return Ok(Some(TerminalOutcome {
                status: TerminalStatus::Interrupted,
                final_text: None,
            }));
        }
        _ => {}
    }
    Ok(None)
}

async fn request_cancel(
    engine: &Engine,
    run_id: RunId,
    cause: CancellationCause,
    state: &mut DriverState,
) -> anyhow::Result<()> {
    state.cancellation_requested = true;
    match engine.cancel_run(run_id).await {
        Ok(result) => {
            if result.cancelled {
                state.cancellation = Some(cause);
            }
            Ok(())
        }
        Err(EngineError::MissingRun(missing)) if missing == run_id => Ok(()),
        Err(error) => Err(anyhow!(error).context("cancel headless run")),
    }
}

async fn reject_escalated(
    engine: &Engine,
    root_session_id: SessionId,
    state: &mut DriverState,
) -> anyhow::Result<bool> {
    let approvals = engine.list_approvals(root_session_id, Some(ApprovalStatus::Escalated));
    let mut rejected_any = false;
    for approval in approvals.approvals {
        let approval_id = approval.request.approval_id();
        if !state.rejected.insert(approval_id) {
            continue;
        }
        let wire = serde_json::to_value(&approval.request)?;
        let request_revision = wire["revision"]
            .as_u64()
            .ok_or_else(|| anyhow!("approval revision is unavailable"))?;
        engine
            .approval_respond(ApprovalRespondParams {
                session_id: approval.session_id,
                approval_id,
                request_revision,
                operation_fingerprint: approval.request.operation_fingerprint().clone(),
                client_response_id: ClientResponseId::new(Uuid::now_v7().to_string())
                    .expect("UUID is a valid client response ID"),
                decision: ApprovalUserDecision::Reject,
                feedback: None,
            })
            .await
            .context("reject escalated headless approval")?;
        state.approval_rejections = state.approval_rejections.saturating_add(1);
        rejected_any = true;
    }
    Ok(rejected_any)
}

fn subscribe_tool_output(
    engine: &Engine,
    call_id: ToolCallId,
    state: &mut DriverState,
    output: &mut dyn IoWrite,
) -> anyhow::Result<()> {
    for stream in [OutputStream::Stdout, OutputStream::Stderr] {
        if let Some((snapshot, receiver)) = engine.subscribe_tool_output(call_id, stream) {
            for delta in snapshot.chunks {
                write_tool_delta(output, &delta)?;
            }
            state.tool_outputs.push(receiver);
        }
    }
    Ok(())
}

fn drain_tool_output(
    state: &mut DriverState,
    mode: OutputMode,
    verbose: bool,
    output: &mut dyn IoWrite,
) -> anyhow::Result<()> {
    if !verbose || mode != OutputMode::Json {
        return Ok(());
    }
    let mut retained = Vec::with_capacity(state.tool_outputs.len());
    for mut receiver in state.tool_outputs.drain(..) {
        loop {
            match receiver.try_recv() {
                Ok(OutputMessage::Delta(delta)) => write_tool_delta(output, &delta)?,
                Ok(OutputMessage::Gap(gap)) => write_tool_gap(output, &gap)?,
                Err(mpsc::error::TryRecvError::Empty) => {
                    retained.push(receiver);
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }
    state.tool_outputs = retained;
    Ok(())
}

fn write_tool_delta(output: &mut dyn IoWrite, delta: &OutputDelta) -> anyhow::Result<()> {
    write_json_line(
        output,
        &JsonToolOutput {
            r#type: "tool_output",
            call_id: delta.call_id,
            stream: delta.stream,
            byte_offset: delta.byte_offset,
            data: &delta.data,
        },
    )
}

fn write_tool_gap(output: &mut dyn IoWrite, gap: &OutputGap) -> anyhow::Result<()> {
    write_json_line(
        output,
        &JsonToolOutputGap {
            r#type: "tool_output_gap",
            call_id: gap.call_id,
            stream: gap.stream,
            next_offset: gap.next_offset,
        },
    )
}

fn terminal_exit(terminal: &TerminalOutcome, cancellation: Option<CancellationCause>) -> i32 {
    match terminal.status {
        TerminalStatus::Completed => 0,
        TerminalStatus::Failed => EXIT_FAILURE,
        TerminalStatus::Cancelled if cancellation == Some(CancellationCause::Permission) => {
            EXIT_PERMISSION
        }
        TerminalStatus::Cancelled | TerminalStatus::Interrupted => EXIT_CANCELLED,
    }
}

fn write_terminal(
    engine: &Engine,
    output: &mut dyn IoWrite,
    mode: OutputMode,
    session_id: SessionId,
    run_id: RunId,
    state: &DriverState,
    result: TerminalResult,
) -> anyhow::Result<()> {
    let TerminalResult {
        outcome: terminal,
        exit_code,
    } = result;
    if mode == OutputMode::Text
        && let Some(text) = &terminal.final_text
    {
        output.write_all(text.as_bytes())?;
        output.write_all(b"\n")?;
    }
    if mode == OutputMode::Json {
        let usage = engine
            .session_usage(session_id)
            .context("load headless usage summary")?
            .usage;
        write_json_line(
            output,
            &JsonSummary {
                r#type: "summary",
                session_id,
                run_id,
                status: terminal.status,
                exit_code,
                turns: state.turns,
                approval_rejections: state.approval_rejections,
                event_recoveries: state.event_recoveries,
                cancellation: state.cancellation,
                final_text: terminal.final_text,
                usage,
            },
        )?;
    }
    output.flush()?;
    Ok(())
}

fn progress(args: &RunArgs, stderr: &mut dyn IoWrite, message: &str) {
    if args.verbose {
        let _ = writeln!(stderr, "cookie run: {message}");
    }
}

fn write_json_line(writer: &mut dyn IoWrite, value: &impl Serialize) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_suppresses_duplicates_and_accepts_persisted_sequence_gaps() {
        let session_id = SessionId::new_v7();
        let event = |seq| StoredEvent {
            engine_version: None,
            session_id,
            run_id: None,
            seq,
            timestamp: "2026-01-01T00:00:00Z".parse().expect("timestamp"),
            payload: EventPayload::UserInputSubmitted {
                input: "cursor test".into(),
            },
        };
        let mut state = DriverState::new(2);
        assert!(matches!(
            accept_event(&mut state, event(2)),
            AcceptedEvent::Duplicate
        ));
        assert!(matches!(
            accept_event(&mut state, event(4)),
            AcceptedEvent::Event(_)
        ));
        assert_eq!(state.cursor, 4);
        assert!(matches!(
            accept_event(&mut state, event(3)),
            AcceptedEvent::Duplicate
        ));
        assert_eq!(state.cursor, 4);
    }

    #[test]
    fn permission_exit_requires_a_confirmed_cancelled_terminal() {
        let cancelled = TerminalOutcome {
            status: TerminalStatus::Cancelled,
            final_text: None,
        };
        let interrupted = TerminalOutcome {
            status: TerminalStatus::Interrupted,
            final_text: None,
        };
        assert_eq!(
            terminal_exit(&cancelled, Some(CancellationCause::Permission)),
            EXIT_PERMISSION
        );
        assert_eq!(
            terminal_exit(&interrupted, Some(CancellationCause::Permission)),
            EXIT_CANCELLED
        );
        assert_eq!(terminal_exit(&cancelled, None), EXIT_CANCELLED);
    }
}
