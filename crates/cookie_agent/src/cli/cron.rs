//! `cookie cron`: list, show, create, edit, delete, enable, disable, and run
//! a daemon's cron jobs with the `crons.*` methods, so it works against a
//! remote daemon (`docs/site/specs/cron.md` section 12).

use anyhow::Context as _;
use clap::{Args, Subcommand, ValueEnum};
use cookie_agent::run::PermissionModeArg;
use cookie_agent_client_state::cron::{
    self as cron, CronDraft, CronDraftTarget, bare_template, daemon_has_crons,
    update_changes_anything,
};
use cookie_agent_protocol::{
    Client, CronActor, CronJob, CronJobId, CronTarget, CronUpdateParams, ProducerDeliveryMode,
    ProjectName, SessionGetParams, SessionId, SessionOrigin,
};
use jiff::{Timestamp, tz::TimeZone};

use super::memory::MemoryIo;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum CronCommand {
    /// List the jobs: id, title, schedule, next fire, target, state, and
    /// the last fire's outcome.
    List,
    /// Show a job: its details, its prompt, and its history.
    Show {
        #[arg(value_parser = parse_job_id)]
        id: CronJobId,
    },
    /// Create a job that fires a prompt into a session (`--session`) or
    /// into a new session per fire (`--agent`).
    Create(CronCreateArgs),
    /// Change a job; only the options given change.
    Edit {
        #[arg(value_parser = parse_job_id)]
        id: CronJobId,
        #[command(flatten)]
        changes: CronEditArgs,
    },
    /// Delete a job; its run sessions stay. Asks first unless `--yes`.
    Rm {
        #[arg(value_parser = parse_job_id)]
        id: CronJobId,
        /// Delete without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Enable a job: its next fire is computed from now and its failure
    /// count resets.
    Enable {
        #[arg(value_parser = parse_job_id)]
        id: CronJobId,
    },
    /// Disable a job; it keeps its history.
    Disable {
        #[arg(value_parser = parse_job_id)]
        id: CronJobId,
    },
    /// Fire a job now, as a scheduled fire would. It counts toward neither
    /// its run limit nor its failures.
    Run {
        #[arg(value_parser = parse_job_id)]
        id: CronJobId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum DeliveryArg {
    Steer,
    Queue,
}

impl From<DeliveryArg> for ProducerDeliveryMode {
    fn from(value: DeliveryArg) -> Self {
        match value {
            DeliveryArg::Steer => Self::Steer,
            DeliveryArg::Queue => Self::Queue,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Args)]
pub(crate) struct CronCreateArgs {
    /// The prompt each fire sends.
    #[arg(long)]
    prompt: String,
    /// A five-field cron expression in the daemon host's local time, such
    /// as `0 3 * * *`, or a delay, such as `in 20m`.
    #[arg(long, allow_hyphen_values = true)]
    schedule: String,
    /// A name lists show instead of the prompt's start.
    #[arg(long)]
    name: Option<String>,
    /// Cron schedules only: how many fires the job delivers, a whole
    /// number, 0 or less for no limit; 1 when omitted.
    #[arg(long, value_name = "RUNS", allow_negative_numbers = true)]
    max_runs: Option<String>,
    /// Fire into this session (root or delegated).
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with = "agent",
        required_unless_present = "agent"
    )]
    session: Option<SessionId>,
    /// In-session jobs: `steer` into a running turn or `queue` after it;
    /// `queue` when omitted.
    #[arg(long, value_enum, conflicts_with = "agent")]
    delivery: Option<DeliveryArg>,
    /// Start a new session with this agent per fire.
    #[arg(long)]
    agent: Option<String>,
    /// New-session jobs: a model pin, `provider/model` or
    /// `provider/model[variant]`.
    #[arg(long, conflicts_with = "session")]
    model: Option<String>,
    /// New-session jobs: the run sessions' permission mode;
    /// `auto-approve-n` when omitted.
    #[arg(long, value_enum, conflicts_with = "session")]
    permission_mode: Option<PermissionModeArg>,
    /// New-session jobs: the project the run sessions belong to.
    #[arg(long, value_parser = super::project::parse_project_name, conflicts_with = "session")]
    project: Option<ProjectName>,
    /// Create the job disabled.
    #[arg(long)]
    disabled: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Args)]
pub(crate) struct CronEditArgs {
    /// A new prompt.
    #[arg(long)]
    prompt: Option<String>,
    /// A new schedule: a cron expression or a delay such as `in 20m`.
    #[arg(long, allow_hyphen_values = true)]
    schedule: Option<String>,
    /// A new name; an empty one removes it.
    #[arg(long)]
    name: Option<String>,
    /// A new run limit: a whole number, 0 or less for no limit.
    #[arg(long, value_name = "RUNS", allow_negative_numbers = true)]
    max_runs: Option<String>,
    /// Fire into this session instead.
    #[arg(long, value_name = "SESSION_ID", conflicts_with = "agent")]
    session: Option<SessionId>,
    /// In-session jobs: `steer` or `queue`.
    #[arg(long, value_enum)]
    delivery: Option<DeliveryArg>,
    /// Start a new session with this agent per fire instead.
    #[arg(long)]
    agent: Option<String>,
    /// New-session jobs: a model pin; an empty one removes it.
    #[arg(long)]
    model: Option<String>,
    /// New-session jobs: the run sessions' permission mode.
    #[arg(long, value_enum)]
    permission_mode: Option<PermissionModeArg>,
}

pub(crate) fn parse_job_id(value: &str) -> Result<CronJobId, String> {
    CronJobId::new(value)
        .map_err(|_| format!("`{value}` is not a job id: eight lowercase letters and digits"))
}

pub(crate) async fn run_cron(url: &str, token: &str, command: CronCommand) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    if let CronCommand::Rm { yes: false, .. } = &command {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "cookie cron rm asks before deleting; pass --yes to delete without a terminal"
        );
    }
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(super::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    run_cron_with(&client, command, &mut super::memory::StdioMemoryIo).await
}

pub(crate) async fn run_cron_with<I: MemoryIo>(
    client: &Client,
    command: CronCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        daemon_has_crons(client.daemon_version()),
        "this daemon does not keep cron jobs: it needs client protocol 1.15 or later"
    );
    let zone = TimeZone::system();
    match command {
        CronCommand::List => {
            let listed = client.list_crons().await.context("crons.list failed")?;
            if !listed.firing {
                io.write_line("Firing is off ([cron] enabled = false): jobs do not fire.")?;
            }
            if listed.jobs.is_empty() {
                return io.write_line("No cron jobs.");
            }
            let titles = SessionTitles::of(client, &listed.jobs).await;
            for job in &listed.jobs {
                for line in job_lines(job, &titles.target(&job.target), Timestamp::now(), &zone) {
                    io.write_line(&line)?;
                }
            }
            Ok(())
        }
        CronCommand::Show { id } => {
            let job = find_job(client, &id).await?;
            let titles = SessionTitles::of(client, std::slice::from_ref(&job)).await;
            for line in show_lines(
                &job,
                |session| titles.title(session),
                Timestamp::now(),
                &zone,
            ) {
                io.write_line(&line)?;
            }
            Ok(())
        }
        CronCommand::Create(args) => {
            let draft = create_draft(client, args).await?;
            let mut params = draft.0.create_params().map_err(anyhow::Error::msg)?;
            if draft.1 {
                params.enabled = Some(false);
            }
            let job = client
                .create_cron(params)
                .await
                .map_err(anyhow::Error::new)?
                .job;
            io.write_line(&format!(
                "Created job {} ({}).",
                job.id,
                cron::job_title(&job)
            ))?;
            next_fire_line(io, &job, &zone)
        }
        CronCommand::Edit { id, changes } => {
            let job = find_job(client, &id).await?;
            let update = edit_update(client, &job, changes, &zone).await?;
            if !update_changes_anything(&update) {
                return io.write_line(&format!("Job {id} is unchanged."));
            }
            let job = client
                .update_cron(update)
                .await
                .map_err(anyhow::Error::new)?
                .job;
            io.write_line(&format!(
                "Updated job {} ({}).",
                job.id,
                cron::job_title(&job)
            ))?;
            next_fire_line(io, &job, &zone)
        }
        CronCommand::Rm { id, yes } => {
            let job = find_job(client, &id).await?;
            let title = cron::job_title(&job);
            if !yes && !io.confirm(&format!("Delete job {id} ({title})? [y/N]"), false)? {
                return io.write_line(&format!("Job {id} was not deleted."));
            }
            client
                .delete_cron(id.clone())
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!("Deleted job {id} ({title})."))
        }
        CronCommand::Enable { id } => set_enabled(client, id, true, io, &zone).await,
        CronCommand::Disable { id } => set_enabled(client, id, false, io, &zone).await,
        CronCommand::Run { id } => {
            let ran = client
                .run_cron(id.clone())
                .await
                .map_err(anyhow::Error::new)?;
            let fire = ran
                .job
                .history
                .last()
                .map(cron::fire_outcome)
                .unwrap_or_else(|| "fired".to_owned());
            match ran.session_id {
                Some(session) => {
                    io.write_line(&format!("Ran job {id}: {fire}, in session {session}."))
                }
                None => io.write_line(&format!("Ran job {id}: {fire}.")),
            }
        }
    }
}

async fn set_enabled<I: MemoryIo>(
    client: &Client,
    id: CronJobId,
    enabled: bool,
    io: &mut I,
    zone: &TimeZone,
) -> anyhow::Result<()> {
    let mut update = CronUpdateParams::new(id.clone());
    update.enabled = Some(enabled);
    let job = client
        .update_cron(update)
        .await
        .map_err(anyhow::Error::new)?
        .job;
    let verb = if enabled { "Enabled" } else { "Disabled" };
    io.write_line(&format!("{verb} job {id} ({}).", cron::job_title(&job)))?;
    next_fire_line(io, &job, zone)
}

fn next_fire_line<I: MemoryIo>(io: &mut I, job: &CronJob, zone: &TimeZone) -> anyhow::Result<()> {
    match job.next_fire {
        Some(next) => io.write_line(&format!(
            "Next fire: {} ({}).",
            cron::local_minute(next, zone),
            cron::until(next, Timestamp::now())
        )),
        None => io.write_line(&format!("No next fire: {}.", cron::state_label(job))),
    }
}

/// The job `id`, from `crons.list`.
async fn find_job(client: &Client, id: &CronJobId) -> anyhow::Result<CronJob> {
    client
        .list_crons()
        .await
        .context("crons.list failed")?
        .jobs
        .into_iter()
        .find(|job| job.id == *id)
        .with_context(|| format!("there is no cron job {id}; see `cookie cron list`"))
}

/// The titles of the sessions jobs fire into and were created in, as far
/// as `session.get` finds them.
struct SessionTitles(Vec<(SessionId, String)>);

impl SessionTitles {
    async fn of(client: &Client, jobs: &[CronJob]) -> Self {
        let mut titles = Vec::new();
        let sessions = jobs.iter().flat_map(|job| {
            let creator = match &job.created_by {
                CronActor::Agent(agent) => Some(agent.session),
                CronActor::User => None,
            };
            job.target.session().into_iter().chain(creator)
        });
        for session in sessions {
            if titles.iter().any(|(known, _)| *known == session) {
                continue;
            }
            if let Ok(got) = client
                .get_session(SessionGetParams {
                    session_id: session,
                })
                .await
                && let Some(title) = got.session.title
            {
                titles.push((session, title.as_str().to_owned()));
            }
        }
        Self(titles)
    }

    fn title(&self, session: SessionId) -> Option<String> {
        self.0
            .iter()
            .find(|(known, _)| *known == session)
            .map(|(_, title)| title.clone())
    }

    fn target(&self, target: &CronTarget) -> String {
        cron::target_label(target, |session| self.title(session))
    }
}

/// A job in `cookie cron list`: a line of its id, title, and state, then
/// an indented line of its schedule, next fire, target, and last outcome.
pub(crate) fn job_lines(
    job: &CronJob,
    target: &str,
    now: Timestamp,
    zone: &TimeZone,
) -> Vec<String> {
    let mut first = format!("{}  {}", job.id, cron::job_title(job));
    if !job.enabled {
        first.push_str(&format!("  ({})", cron::state_label(job)));
    }
    let next = match job.next_fire {
        Some(next) => format!(
            "next {} ({})",
            cron::local_minute(next, zone),
            cron::until(next, now)
        ),
        None => "no next fire".to_owned(),
    };
    vec![
        first,
        format!(
            "    {} · {next} · {target} · last {}",
            cron::schedule_reading(&job.schedule, job.max_runs, zone),
            cron::last_outcome(job)
        ),
    ]
}

/// `cookie cron show`: the job's details, its prompt, and its history,
/// newest first.
pub(crate) fn show_lines(
    job: &CronJob,
    title: impl Fn(SessionId) -> Option<String>,
    now: Timestamp,
    zone: &TimeZone,
) -> Vec<String> {
    let mut lines = vec![cron::job_title(job)];
    lines.extend(
        cron::detail_lines(job, now, zone, title)
            .into_iter()
            .map(|detail| match detail.session {
                Some(session) => format!("{}: {} · session {session}", detail.label, detail.value),
                None => format!("{}: {}", detail.label, detail.value),
            }),
    );
    lines.push(String::new());
    lines.extend(job.prompt.lines().map(str::to_owned));
    lines.push(String::new());
    if job.history.is_empty() {
        lines.push("No fires yet.".to_owned());
    } else {
        lines.push("History:".to_owned());
        for fire in job.history.iter().rev() {
            let mut line = format!(
                "  {}  {}",
                cron::local_minute(fire.fired_at, zone),
                cron::fire_outcome(fire)
            );
            if let Some(session) = fire.session_id {
                line.push_str(&format!(" · session {session}"));
            }
            lines.push(line);
        }
    }
    lines
}

/// Whether `session` is a delegated session.
async fn is_subagent(client: &Client, session: SessionId) -> anyhow::Result<bool> {
    let got = client
        .get_session(SessionGetParams {
            session_id: session,
        })
        .await
        .map_err(anyhow::Error::new)
        .with_context(|| format!("session {session}"))?;
    Ok(matches!(
        got.session.origin,
        SessionOrigin::Delegated { .. }
    ))
}

fn parse_agent(agent: &str) -> anyhow::Result<cookie_agent_protocol::AgentId> {
    agent
        .parse()
        .map_err(|_| anyhow::anyhow!("`{agent}` is not an agent id"))
}

/// The form `cookie cron create` fills, and whether the job starts
/// disabled.
async fn create_draft(client: &Client, args: CronCreateArgs) -> anyhow::Result<(CronDraft, bool)> {
    let mut draft = match (args.session, &args.agent) {
        (Some(session), _) => CronDraft::for_session(session, is_subagent(client, session).await?),
        (None, Some(agent)) => {
            let mut template = bare_template(parse_agent(agent)?);
            if let Some(name) = &args.project {
                template.project = Some(super::project::find_project(client, name).await?.id);
            }
            let mut draft = CronDraft::for_new_session(template);
            draft.model = args.model.unwrap_or_default();
            draft.permission_mode = args.permission_mode.map(Into::into);
            draft
        }
        (None, None) => anyhow::bail!("give --session or --agent"),
    };
    draft.prompt = args.prompt;
    draft.schedule = args.schedule;
    draft.name = args.name.unwrap_or_default();
    draft.max_runs = args.max_runs.unwrap_or_default();
    if let Some(delivery) = args.delivery {
        draft.delivery = delivery.into();
    }
    Ok((draft, args.disabled))
}

/// The update `cookie cron edit` sends: the job's form with the options
/// given applied, against the job.
async fn edit_update(
    client: &Client,
    job: &CronJob,
    changes: CronEditArgs,
    zone: &TimeZone,
) -> anyhow::Result<CronUpdateParams> {
    let mut draft = CronDraft::of_job(job, zone);
    if let Some(session) = changes.session {
        draft.target = CronDraftTarget::Session {
            session,
            subagent: is_subagent(client, session).await?,
        };
    }
    if let Some(agent) = changes.agent {
        if !draft.target.is_new() {
            draft.target = CronDraftTarget::New {
                template: bare_template(parse_agent(&agent)?),
            };
        }
        draft.agent = agent;
    }
    let new = draft.target.is_new();
    if let Some(model) = changes.model {
        anyhow::ensure!(
            new,
            "--model is for a job that starts a new session per fire"
        );
        draft.model = model;
    }
    if let Some(mode) = changes.permission_mode {
        anyhow::ensure!(
            new,
            "--permission-mode is for a job that starts a new session per fire"
        );
        draft.permission_mode = Some(mode.into());
    }
    if let Some(delivery) = changes.delivery {
        anyhow::ensure!(!new, "--delivery is for a job that fires into a session");
        draft.delivery = delivery.into();
    }
    if let Some(prompt) = changes.prompt {
        draft.prompt = prompt;
    }
    if let Some(schedule) = changes.schedule {
        draft.schedule = schedule;
    }
    if let Some(name) = changes.name {
        draft.name = name;
    }
    if let Some(max_runs) = changes.max_runs {
        draft.max_runs = max_runs;
    }
    draft.update_params(job, zone).map_err(anyhow::Error::msg)
}
