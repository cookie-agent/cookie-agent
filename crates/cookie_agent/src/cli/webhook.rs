//! `cookie webhook`: list, show, create, edit, delete, enable, disable,
//! rotate, and test a daemon's webhooks with the `webhooks.*` methods, so
//! it works against a remote daemon (`docs/site/specs/webhooks.md`
//! section 14).

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use cookie_agent::run::PermissionModeArg;
use cookie_agent_client_state::webhook::{
    self as webhook, WebhookDraft, WebhookDraftTarget, daemon_has_webhooks, update_changes_anything,
};
use cookie_agent_protocol::{
    CronActor, ProjectInfo, ProjectName, SessionGetParams, SessionId, SessionOrigin, Webhook,
    WebhookHeader, WebhookId, WebhookRotate, WebhookTarget, WebhookTemplate, WebhookTestParams,
    WebhookUpdateParams,
};
use cookie_agent_protocol_transport::Client;
use jiff::tz::TimeZone;

use super::cron::DeliveryArg;
use super::memory::MemoryIo;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum WebhookCommand {
    /// List the webhooks: id, title, state, check, target, request counts,
    /// the last event's outcome, and the URL.
    List,
    /// Show a webhook: its details, prompts, threads, and recent events.
    Show {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
    },
    /// Create a webhook that delivers each request into a session
    /// (`--session`) or into new sessions (`--agent`). Prints its URL, and
    /// its signing secret once.
    Create(WebhookCreateArgs),
    /// Change a webhook; only the options given change.
    Edit {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
        #[command(flatten)]
        changes: WebhookEditArgs,
    },
    /// Delete a webhook; its sessions stay. Asks first unless `--yes`.
    Rm {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
        /// Delete without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Enable a webhook; its failure count resets.
    Enable {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
    },
    /// Disable a webhook: its URL answers 404 until it is enabled again.
    Disable {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
    },
    /// Give a webhook a new URL (`url`) or a new signing secret
    /// (`secret`), printed once.
    Rotate {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
        /// `url`: a new URL, and the old one stops working; `secret`: a new
        /// signing secret, which senders need before their next request.
        #[arg(value_enum)]
        what: RotateArg,
    },
    /// Print a webhook's signing secret.
    Secret {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
    },
    /// Deliver a test event, skipping the check, the filter, and the
    /// limits.
    Test {
        #[arg(value_parser = parse_webhook_id)]
        id: WebhookId,
        /// A file whose text is the request body; `-` reads standard input.
        #[arg(long, value_name = "FILE")]
        body: Option<PathBuf>,
        /// The body's content type, such as `application/json`.
        #[arg(long)]
        content_type: Option<String>,
        /// A request header, `Name: value`; may be given more than once.
        #[arg(long = "header", value_name = "NAME: VALUE", value_parser = parse_header)]
        headers: Vec<WebhookHeader>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum RotateArg {
    Url,
    Secret,
}

#[derive(Debug, PartialEq, Eq, Args)]
#[command(group(
    clap::ArgGroup::new("delivered_into")
        .args(["session", "thread_key"])
        .multiple(true)
))]
pub(crate) struct WebhookCreateArgs {
    /// A name lists show instead of the id.
    #[arg(long)]
    name: Option<String>,
    /// Instructions sent with every event.
    #[arg(long)]
    prompt: Option<String>,
    /// Deliver into this session (root or delegated).
    #[arg(
        long,
        value_name = "SESSION_ID",
        conflicts_with = "agent",
        required_unless_present = "agent"
    )]
    session: Option<SessionId>,
    /// Start new sessions with this agent.
    #[arg(long)]
    agent: Option<String>,
    /// Session targets and thread keys: `steer` into a running turn or
    /// `queue` after it; `queue` when omitted.
    #[arg(long, value_enum, requires = "delivered_into")]
    delivery: Option<DeliveryArg>,
    /// New sessions: a model pin, `provider/model` or
    /// `provider/model[variant]`.
    #[arg(long, conflicts_with = "session")]
    model: Option<String>,
    /// New sessions: their permission mode; `auto-approve-n` when omitted.
    #[arg(long, value_enum, conflicts_with = "session")]
    permission_mode: Option<PermissionModeArg>,
    /// New sessions: the project they belong to.
    #[arg(long, value_parser = super::project::parse_project_name, conflicts_with = "session")]
    project: Option<ProjectName>,
    /// New sessions: a thread key, `json:/pointer` or `header:Name`; events
    /// with the same key share a session.
    #[arg(long, value_name = "SOURCE", conflicts_with = "session")]
    thread_key: Option<String>,
    /// New sessions: instructions each session gets once, and again after
    /// each compaction.
    #[arg(long, value_name = "TEXT", conflicts_with = "session")]
    thread_prompt: Option<String>,
    /// A signature check: `github`, `standard`, `bearer`, or
    /// `hmac_sha256 header=<name> encoding=hex|base64 prefix=<text>`.
    #[arg(long, value_name = "CHECK")]
    check: Option<String>,
    /// The check's secret; the daemon makes one when omitted.
    #[arg(long, requires = "check", allow_hyphen_values = true)]
    secret: Option<String>,
    /// A condition every request must meet, `header:Name=a,b` or
    /// `json:/pointer=a,b`; may be given more than once.
    #[arg(long, value_name = "CONDITION")]
    filter: Vec<String>,
    /// A request header the model sees besides the usual ones; may be given
    /// more than once.
    #[arg(long, value_name = "NAME")]
    show_header: Vec<String>,
    /// Events accepted per rolling hour; 0 or less for no limit.
    #[arg(long, value_name = "COUNT", allow_negative_numbers = true)]
    max_per_hour: Option<String>,
    /// New sessions: how many may run at once; 0 or less for no limit.
    #[arg(
        long,
        value_name = "COUNT",
        allow_negative_numbers = true,
        conflicts_with = "session"
    )]
    max_concurrent: Option<String>,
    /// Failures in a row that disable the webhook; 0 or less never.
    #[arg(long, value_name = "COUNT", allow_negative_numbers = true)]
    max_failures: Option<String>,
    /// Create the webhook disabled.
    #[arg(long)]
    disabled: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Args)]
pub(crate) struct WebhookEditArgs {
    /// A new name; an empty one removes it.
    #[arg(long)]
    name: Option<String>,
    /// New instructions sent with every event; an empty one removes them.
    #[arg(long)]
    prompt: Option<String>,
    /// Deliver into this session instead.
    #[arg(long, value_name = "SESSION_ID", conflicts_with = "agent")]
    session: Option<SessionId>,
    /// Start new sessions with this agent instead.
    #[arg(long)]
    agent: Option<String>,
    /// Session targets and thread keys: `steer` or `queue`.
    #[arg(long, value_enum)]
    delivery: Option<DeliveryArg>,
    /// New sessions: a model pin; an empty one removes it.
    #[arg(long)]
    model: Option<String>,
    /// New sessions: their permission mode.
    #[arg(long, value_enum)]
    permission_mode: Option<PermissionModeArg>,
    /// New sessions: a thread key; an empty one makes each event start a
    /// session again.
    #[arg(long, value_name = "SOURCE")]
    thread_key: Option<String>,
    /// New sessions: the thread prompt; an empty one removes it.
    #[arg(long, value_name = "TEXT")]
    thread_prompt: Option<String>,
    /// A new check; an empty one removes it, leaving the URL alone.
    #[arg(long, value_name = "CHECK")]
    check: Option<String>,
    /// A new secret for the check.
    #[arg(long, allow_hyphen_values = true)]
    secret: Option<String>,
    /// Conditions that replace the filter; may be given more than once.
    #[arg(long, value_name = "CONDITION", conflicts_with = "no_filter")]
    filter: Vec<String>,
    /// Remove the filter.
    #[arg(long)]
    no_filter: bool,
    /// Header names that replace the extra ones the model sees.
    #[arg(long, value_name = "NAME", conflicts_with = "no_show_headers")]
    show_header: Vec<String>,
    /// Show the model only the usual headers.
    #[arg(long)]
    no_show_headers: bool,
    /// A new hourly limit; empty for the daemon's default, 0 or less for none.
    #[arg(long, value_name = "COUNT", allow_negative_numbers = true)]
    max_per_hour: Option<String>,
    /// A new limit on sessions running at once.
    #[arg(long, value_name = "COUNT", allow_negative_numbers = true)]
    max_concurrent: Option<String>,
    /// A new failure limit.
    #[arg(long, value_name = "COUNT", allow_negative_numbers = true)]
    max_failures: Option<String>,
}

pub(crate) fn parse_webhook_id(value: &str) -> Result<WebhookId, String> {
    WebhookId::new(value.to_owned())
        .map_err(|_| format!("`{value}` is not a webhook id: eight lowercase letters and digits"))
}

fn parse_header(value: &str) -> Result<WebhookHeader, String> {
    let (name, header_value) = value
        .split_once(':')
        .ok_or_else(|| format!("`{value}` is not `Name: value`"))?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err(format!("`{value}` names no header"));
    }
    Ok(WebhookHeader {
        name: name.to_owned(),
        value: header_value.trim().to_owned(),
    })
}

pub(crate) async fn run_webhook(
    url: &str,
    token: &str,
    command: WebhookCommand,
) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    if let WebhookCommand::Rm { yes: false, .. } = &command {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "cookie webhook rm asks before deleting; pass --yes to delete without a terminal"
        );
    }
    let client = Client::connect_websocket_with_token(url, token).await?;
    super::handshake_ready(&client, super::client_identity("cli")).await?;
    let page = cookie_agent_client_state::shares::page_base(url).unwrap_or_default();
    run_webhook_with(&client, &page, command, &mut super::memory::StdioMemoryIo).await
}

/// Runs `command` against `client`; `page` is the address of the daemon's
/// page, which URLs use when the daemon has no public address.
pub(crate) async fn run_webhook_with<I: MemoryIo>(
    client: &Client,
    page: &str,
    command: WebhookCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        daemon_has_webhooks(client.daemon_version()),
        "this daemon does not keep webhooks: it needs client protocol 1.96 or later"
    );
    let zone = TimeZone::system();
    let listed = client
        .list_webhooks()
        .await
        .context("webhooks.list failed")?;
    let base = listed.public_base.clone();
    let url_of = |hook: &Webhook| webhook::webhook_url(base.as_deref(), page, &hook.path);
    let find = |id: &WebhookId| -> anyhow::Result<Webhook> {
        listed
            .webhooks
            .iter()
            .find(|hook| hook.id == *id)
            .cloned()
            .with_context(|| format!("there is no webhook {id}; see `cookie webhook list`"))
    };
    match command {
        WebhookCommand::List => {
            if !listed.receiving {
                io.write_line(
                    "Receiving is off ([webhooks] enabled = false): every URL answers 404.",
                )?;
            }
            if listed.webhooks.is_empty() {
                return io.write_line("No webhooks.");
            }
            let names = HookNames::of(client, &listed.webhooks).await;
            for hook in &listed.webhooks {
                for line in list_lines(hook, &names.target(&hook.target), &url_of(hook)) {
                    io.write_line(&line)?;
                }
            }
            Ok(())
        }
        WebhookCommand::Show { id } => {
            let hook = find(&id)?;
            let names = HookNames::of(client, std::slice::from_ref(&hook)).await;
            for line in show_lines(&hook, &url_of(&hook), &names, &zone) {
                io.write_line(&line)?;
            }
            Ok(())
        }
        WebhookCommand::Create(args) => {
            let disabled = args.disabled;
            let draft = create_draft(client, args).await?;
            let mut params = draft.create_params().map_err(anyhow::Error::msg)?;
            if disabled {
                params.enabled = Some(false);
            }
            let created = client
                .create_webhook(params)
                .await
                .map_err(anyhow::Error::new)?;
            let hook = created.webhook;
            io.write_line(&format!(
                "Created webhook {} ({}).",
                hook.id,
                webhook::webhook_title(&hook)
            ))?;
            url_lines(io, &url_of(&hook), base.as_deref(), page)?;
            secret_line(io, created.secret.as_deref())
        }
        WebhookCommand::Edit { id, changes } => {
            let hook = find(&id)?;
            let update = edit_update(client, &hook, changes).await?;
            if !update_changes_anything(&update) {
                return io.write_line(&format!("Webhook {id} is unchanged."));
            }
            let updated = client
                .update_webhook(update)
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!(
                "Updated webhook {} ({}).",
                updated.webhook.id,
                webhook::webhook_title(&updated.webhook)
            ))?;
            secret_line(io, updated.secret.as_deref())
        }
        WebhookCommand::Rm { id, yes } => {
            let hook = find(&id)?;
            let title = webhook::webhook_title(&hook);
            if !yes && !io.confirm(&format!("Delete webhook {id} ({title})? [y/N]"), false)? {
                return io.write_line(&format!("Webhook {id} was not deleted."));
            }
            client
                .delete_webhook(id.clone())
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!("Deleted webhook {id} ({title})."))
        }
        WebhookCommand::Enable { id } => set_enabled(client, id, true, io).await,
        WebhookCommand::Disable { id } => set_enabled(client, id, false, io).await,
        WebhookCommand::Rotate { id, what } => {
            let what = match what {
                RotateArg::Url => WebhookRotate::Url,
                RotateArg::Secret => WebhookRotate::Secret,
            };
            let rotated = client
                .rotate_webhook(id.clone(), what)
                .await
                .map_err(anyhow::Error::new)?;
            match what {
                WebhookRotate::Url => {
                    io.write_line(&format!(
                        "Webhook {id} has a new URL; the old one stops working."
                    ))?;
                    url_lines(io, &url_of(&rotated.webhook), base.as_deref(), page)
                }
                WebhookRotate::Secret => {
                    io.write_line(&format!("Webhook {id} has a new secret."))?;
                    secret_line(io, rotated.secret.as_deref())
                }
            }
        }
        WebhookCommand::Secret { id } => {
            let secret = client
                .webhook_secret(id.clone())
                .await
                .map_err(anyhow::Error::new)?
                .secret;
            match secret {
                Some(secret) => io.write_line(&secret),
                None => io.write_line(&format!("Webhook {id} has no secret: it has no check.")),
            }
        }
        WebhookCommand::Test {
            id,
            body,
            content_type,
            headers,
        } => {
            let body = match body {
                None => None,
                Some(path) if path.as_os_str() == "-" => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                        .context("reading the body from standard input")?;
                    Some(text)
                }
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?,
                ),
            };
            let tested = client
                .test_webhook(WebhookTestParams {
                    id: id.clone(),
                    body,
                    content_type,
                    headers,
                })
                .await
                .map_err(anyhow::Error::new)?;
            let outcome = tested
                .webhook
                .history
                .iter()
                .find(|event| event.id == tested.event)
                .map(webhook::event_outcome)
                .unwrap_or_else(|| "stored".to_owned());
            io.write_line(&format!(
                "Sent test event {} to webhook {id}: {outcome}.",
                tested.event
            ))
        }
    }
}

async fn set_enabled<I: MemoryIo>(
    client: &Client,
    id: WebhookId,
    enabled: bool,
    io: &mut I,
) -> anyhow::Result<()> {
    let mut update = WebhookUpdateParams::new(id.clone());
    update.enabled = Some(enabled);
    let hook = client
        .update_webhook(update)
        .await
        .map_err(anyhow::Error::new)?
        .webhook;
    let verb = if enabled { "Enabled" } else { "Disabled" };
    io.write_line(&format!(
        "{verb} webhook {id} ({}).",
        webhook::webhook_title(&hook)
    ))
}

/// The URL, and a warning when it is probably reachable only privately.
fn url_lines<I: MemoryIo>(
    io: &mut I,
    url: &str,
    public_base: Option<&str>,
    page: &str,
) -> anyhow::Result<()> {
    io.write_line(&format!("URL: {url}"))?;
    if webhook::url_looks_private(public_base, page) {
        io.write_line(
            "This URL works only where cookie's page can be reached; set [public] url for a public one.",
        )?;
    }
    Ok(())
}

fn secret_line<I: MemoryIo>(io: &mut I, secret: Option<&str>) -> anyhow::Result<()> {
    match secret {
        Some(secret) => io.write_line(&format!(
            "Secret (shown once; `cookie webhook secret` prints it again): {secret}"
        )),
        None => Ok(()),
    }
}

/// The titles of the sessions webhooks deliver into and were created in,
/// as far as `session.get` finds them, and the projects their new sessions
/// go in.
struct HookNames {
    titles: Vec<(SessionId, String)>,
    projects: Vec<ProjectInfo>,
}

impl HookNames {
    async fn of(client: &Client, hooks: &[Webhook]) -> Self {
        let mut titles = Vec::new();
        let sessions = hooks.iter().flat_map(|hook| {
            let creator = match &hook.created_by {
                CronActor::Agent(agent) => Some(agent.session),
                CronActor::User => None,
            };
            hook.target
                .session()
                .into_iter()
                .chain(creator)
                .chain(hook.threads.iter().map(|thread| thread.session_id))
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
        let names_project = hooks.iter().any(|hook| match &hook.target {
            WebhookTarget::New(template) => template.project.is_some(),
            _ => false,
        });
        let projects = if names_project {
            client
                .list_projects()
                .await
                .map(|listed| listed.projects)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Self { titles, projects }
    }

    fn title(&self, session: SessionId) -> Option<String> {
        self.titles
            .iter()
            .find(|(known, _)| *known == session)
            .map(|(_, title)| title.clone())
    }

    fn target(&self, target: &WebhookTarget) -> String {
        webhook::target_label(target, &self.projects, |session| self.title(session))
    }
}

/// A webhook in `cookie webhook list`: a line of its id, title, and state,
/// an indented line of its check, target, counts, and last outcome, and an
/// indented line of its URL.
fn list_lines(hook: &Webhook, target: &str, url: &str) -> Vec<String> {
    let mut first = format!("{}  {}", hook.id, webhook::webhook_title(hook));
    if !hook.enabled {
        first.push_str(&format!("  ({})", webhook::state_label(hook)));
    }
    let last = hook
        .history
        .last()
        .map_or_else(|| "never".to_owned(), webhook::event_outcome);
    vec![
        first,
        format!(
            "    {} · {target} · {} · last {last}",
            webhook::check_label(hook.verify.as_ref()),
            webhook::counts_summary(&hook.counts)
        ),
        format!("    {url}"),
    ]
}

/// `cookie webhook show`: the details, the prompts, the threads, and the
/// events, newest first.
fn show_lines(hook: &Webhook, url: &str, names: &HookNames, zone: &TimeZone) -> Vec<String> {
    let mut lines = vec![webhook::webhook_title(hook)];
    lines.extend(
        webhook::detail_lines(hook, url, zone, &names.projects, |session| {
            names.title(session)
        })
        .into_iter()
        .map(|detail| match detail.session {
            Some(session) => format!("{}: {} · session {session}", detail.label, detail.value),
            None => format!("{}: {}", detail.label, detail.value),
        }),
    );
    lines.push(String::new());
    if let Some(prompt) = &hook.prompt {
        lines.push("Prompt:".to_owned());
        lines.extend(prompt.lines().map(|line| format!("  {line}")));
        lines.push(String::new());
    }
    if let WebhookTarget::New(template) = &hook.target
        && let Some(thread_prompt) = &template.thread_prompt
    {
        lines.push("Thread prompt:".to_owned());
        lines.extend(thread_prompt.lines().map(|line| format!("  {line}")));
        lines.push(String::new());
    }
    if !hook.threads.is_empty() {
        lines.push("Threads:".to_owned());
        for thread in hook.threads.iter().rev() {
            lines.push(format!(
                "  {} · session {} · used {}",
                thread.key,
                names
                    .title(thread.session_id)
                    .unwrap_or_else(|| thread.session_id.to_string()),
                webhook::minute(thread.last_used, zone)
            ));
        }
        lines.push(String::new());
    }
    if hook.history.is_empty() {
        lines.push("No events yet.".to_owned());
    } else {
        lines.push("Events:".to_owned());
        for event in webhook::events_newest_first(hook) {
            let mut line = format!(
                "  {}  {}  {}",
                webhook::event_time(event, zone),
                event.id,
                webhook::event_outcome(event)
            );
            if let Some(thread) = &event.thread_key {
                line.push_str(&format!(" · thread {thread}"));
            }
            if let Some(session) = event.session_id {
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

/// The form `cookie webhook create` fills.
async fn create_draft(client: &Client, args: WebhookCreateArgs) -> anyhow::Result<WebhookDraft> {
    let mut draft = match (args.session, &args.agent) {
        (Some(session), _) => {
            WebhookDraft::for_session(session, is_subagent(client, session).await?)
        }
        (None, Some(agent)) => {
            let mut template = WebhookTemplate::for_agent(parse_agent(agent)?);
            if let Some(name) = &args.project {
                template.project = Some(super::project::find_project(client, name).await?.id);
            }
            let mut draft = WebhookDraft::for_new_session(template);
            draft.model = args.model.unwrap_or_default();
            draft.permission_mode = args.permission_mode.map(Into::into);
            draft.thread_key = args.thread_key.unwrap_or_default();
            draft.thread_prompt = args.thread_prompt.unwrap_or_default();
            draft.max_concurrent = args.max_concurrent.unwrap_or_default();
            draft
        }
        (None, None) => anyhow::bail!("give --session or --agent"),
    };
    draft.name = args.name.unwrap_or_default();
    draft.prompt = args.prompt.unwrap_or_default();
    draft.verify = args.check.unwrap_or_default();
    draft.secret = args.secret.unwrap_or_default();
    draft.filter = args.filter.join("\n");
    draft.headers = args.show_header.join(",");
    draft.max_per_hour = args.max_per_hour.unwrap_or_default();
    draft.max_consecutive_failures = args.max_failures.unwrap_or_default();
    if let Some(delivery) = args.delivery {
        anyhow::ensure!(
            draft.takes_delivery(),
            "--delivery is for a webhook that delivers into a session or has a --thread-key"
        );
        draft.delivery = delivery.into();
    }
    Ok(draft)
}

/// The update `cookie webhook edit` sends: the webhook's form with the
/// options given applied, against the webhook.
async fn edit_update(
    client: &Client,
    hook: &Webhook,
    changes: WebhookEditArgs,
) -> anyhow::Result<WebhookUpdateParams> {
    let mut draft = WebhookDraft::of_webhook(hook);
    if let Some(session) = changes.session {
        draft.target = WebhookDraftTarget::Session {
            session,
            subagent: is_subagent(client, session).await?,
        };
        draft.thread_key.clear();
        draft.thread_prompt.clear();
        draft.max_concurrent.clear();
    }
    if let Some(agent) = changes.agent {
        if !draft.target.is_new() {
            draft.target = WebhookDraftTarget::New {
                template: WebhookTemplate::for_agent(parse_agent(&agent)?),
            };
        }
        draft.agent = agent;
    }
    let new = draft.target.is_new();
    for (flag, value, field) in [
        ("--model", changes.model, &mut draft.model),
        ("--thread-key", changes.thread_key, &mut draft.thread_key),
        (
            "--thread-prompt",
            changes.thread_prompt,
            &mut draft.thread_prompt,
        ),
        (
            "--max-concurrent",
            changes.max_concurrent,
            &mut draft.max_concurrent,
        ),
    ] {
        if let Some(value) = value {
            anyhow::ensure!(new, "{flag} is for a webhook that starts new sessions");
            *field = value;
        }
    }
    if let Some(mode) = changes.permission_mode {
        anyhow::ensure!(
            new,
            "--permission-mode is for a webhook that starts new sessions"
        );
        draft.permission_mode = Some(mode.into());
    }
    if let Some(delivery) = changes.delivery {
        anyhow::ensure!(
            draft.takes_delivery(),
            "--delivery is for a webhook that delivers into a session or has a thread key"
        );
        draft.delivery = delivery.into();
    }
    if let Some(name) = changes.name {
        draft.name = name;
    }
    if let Some(prompt) = changes.prompt {
        draft.prompt = prompt;
    }
    if let Some(check) = changes.check {
        draft.verify = check;
    }
    if let Some(secret) = changes.secret {
        draft.secret = secret;
    }
    if changes.no_filter {
        draft.filter.clear();
    } else if !changes.filter.is_empty() {
        draft.filter = changes.filter.join("\n");
    }
    if changes.no_show_headers {
        draft.headers.clear();
    } else if !changes.show_header.is_empty() {
        draft.headers = changes.show_header.join(",");
    }
    if let Some(limit) = changes.max_per_hour {
        draft.max_per_hour = limit;
    }
    if let Some(limit) = changes.max_failures {
        draft.max_consecutive_failures = limit;
    }
    draft.update_params(hook).map_err(anyhow::Error::msg)
}
