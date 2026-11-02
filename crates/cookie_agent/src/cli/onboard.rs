//! `cookie onboard`: set up a daemon, local or remote, through its
//! onboarding API (`docs/site/specs/default-models-and-onboarding.md`
//! section 7, and `docs/site/specs/machines-on-demand.md` section 2.2):
//! connect a provider when none is, choose `default_model`, and optionally
//! `small_model` and `utility_model`. The daemon fetches its official agents
//! as it finishes; there is no starter kit. It writes nothing itself; the
//! daemon does. Instead of the models, or beside them, it links an ACP agent
//! (Claude Code, Codex, or another), adds MCP servers that need no login, and
//! sets the default agent (`docs/site/specs/acp-onboarding.md` section 9).

use anyhow::Context as _;
use clap::Args;
use cookie_agent_client_state::onboarding::{
    self as rules, AcpPlace, ModelGroup, configured_label, default_model_variant, entry_label,
    field_entry, model_entry, model_groups, parse_model_flag, small_model_variant,
    utility_model_variant,
};
use cookie_agent_protocol::{
    AcpCheckParams, AgentId, AvailableModelDescriptor, OnboardingAcpAgentInput,
    OnboardingMcpServerInput, OnboardingModelEntry, OnboardingModelField, OnboardingRequirement,
    OnboardingRunParams, OnboardingStatus, RuntimeId, VariantId,
};
use cookie_agent_protocol_transport::Client;

use super::connect::ConnectIo;

/// The flags of `cookie onboard`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Args)]
pub(crate) struct OnboardArgs {
    /// Answer every question with its default: keep what is set, leave the
    /// optional models alone. Refuses rather than guess a model it was not
    /// given.
    #[arg(long)]
    pub(crate) yes: bool,
    /// The default model, `provider/model` or `provider/model:variant`;
    /// repeat it for a fallback list, in order.
    #[arg(long, value_name = "KEY[:VARIANT]")]
    pub(crate) default_model: Vec<String>,
    /// The small model, for subagents that carry out work,
    /// `provider/model`, `provider/model:variant`, or `default_model`;
    /// repeat it for a fallback list, in order.
    #[arg(long, value_name = "KEY[:VARIANT]|FIELD")]
    pub(crate) small_model: Vec<String>,
    /// The utility model, for approvals and session titles,
    /// `provider/model`, `provider/model:variant`, `default_model`, or
    /// `small_model`; repeat it for a fallback list, in order.
    #[arg(long, value_name = "KEY[:VARIANT]|FIELD")]
    pub(crate) utility_model: Vec<String>,
    /// Link an ACP agent: a preset's ID (`claude`, `codex`) or `other`
    /// with `--acp-id` and `--acp-command`.
    #[arg(long, value_name = "PRESET|other")]
    pub(crate) acp: Option<String>,
    /// Where the ACP agent runs: `every` machine, or `local`, this daemon's
    /// machine only (the default).
    #[arg(long, value_name = "every|local")]
    pub(crate) acp_where: Option<String>,
    /// The ACP agent's ID; a preset's own by default.
    #[arg(long, value_name = "ID")]
    pub(crate) acp_id: Option<String>,
    /// The command line that starts the ACP agent, in place of a preset's.
    #[arg(long, value_name = "COMMAND")]
    pub(crate) acp_command: Option<String>,
    /// An environment variable of the ACP agent; repeat it.
    #[arg(long, value_name = "KEY=VALUE")]
    pub(crate) acp_env: Vec<String>,
    /// Start the ACP agent once set up, to see that it works.
    #[arg(long)]
    pub(crate) acp_check: bool,
    /// Add MCP servers from the presets that need no login, by ID
    /// (`exa,context7,deepwiki,playwright`).
    #[arg(long, value_name = "ID,...", value_delimiter = ',')]
    pub(crate) mcp: Vec<String>,
    /// The runtime that runs the MCP presets that run a program, such as
    /// Playwright; `local` by default.
    #[arg(long, value_name = "RUNTIME")]
    pub(crate) mcp_runtime: Option<String>,
    /// The agent new sessions start with (`[agents] default`).
    #[arg(long, value_name = "AGENT")]
    pub(crate) default_agent: Option<String>,
}

/// Which ways of running agents the user chose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Paths {
    models: bool,
    acp: bool,
}

/// Which model field a question is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Default,
    Small,
    Utility,
}

impl Slot {
    /// The fields an entry of this field may name: the ones asked before it.
    const fn earlier(self) -> &'static [(char, OnboardingModelField)] {
        match self {
            Self::Default => &[],
            Self::Small => &[('d', OnboardingModelField::DefaultModel)],
            Self::Utility => &[
                ('d', OnboardingModelField::DefaultModel),
                ('s', OnboardingModelField::SmallModel),
            ],
        }
    }

    const fn wire(self) -> OnboardingModelField {
        match self {
            Self::Default => OnboardingModelField::DefaultModel,
            Self::Small => OnboardingModelField::SmallModel,
            Self::Utility => OnboardingModelField::UtilityModel,
        }
    }

    const fn field(self) -> &'static str {
        match self {
            Self::Default => "default_model",
            Self::Small => "small_model",
            Self::Utility => "utility_model",
        }
    }

    /// Whether the daemon needs the model to be set up.
    const fn required(self) -> bool {
        matches!(self, Self::Default)
    }

    const fn number(self) -> usize {
        match self {
            Self::Default => 2,
            Self::Small => 3,
            Self::Utility => 4,
        }
    }

    const fn about(self) -> &'static str {
        match self {
            Self::Default => "the model root agents run on",
            Self::Small => {
                "optional, the model of subagents that carry out work; Enter skips it and they use the default model"
            }
            Self::Utility => {
                "optional, a fast, cheap model for approvals and titles; Enter skips it and they use the session's model"
            }
        }
    }
}

/// `cookie onboard` against the daemon at `url`.
pub(crate) async fn run_onboard(url: &str, token: &str, args: OnboardArgs) -> anyhow::Result<()> {
    if !args.yes {
        use std::io::IsTerminal as _;
        super::connect::require_interactive_tty(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
            "onboard",
        )?;
    }
    let client = Client::connect_websocket_with_token(url, token).await?;
    super::handshake_ready(&client, super::client_identity("cli")).await?;
    let mut io = super::connect::StdioConnectIo;
    run_onboard_with(&client, args, &mut io).await
}

/// `cookie onboard` over `client`, which has made its handshake.
pub(crate) async fn run_onboard_with(
    client: &Client,
    args: OnboardArgs,
    io: &mut impl ConnectIo,
) -> anyhow::Result<()> {
    let flags = |values: &[String], flag: &str| {
        values
            .iter()
            .map(|value| parse_model_flag(value))
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::msg)
            .context(flag.to_owned())
            .map(|entries| (!entries.is_empty()).then_some(entries))
    };
    let default_flag = flags(&args.default_model, "--default-model")?;
    let small_flag = flags(&args.small_model, "--small-model")?;
    let utility_flag = flags(&args.utility_model, "--utility-model")?;
    let mut status = client.get_onboarding().await.context("onboarding.get")?;
    describe_status(&status, io)?;
    let paths = choose_paths(&args, &status, io)?;

    // The ACP agent first: with one, the models are optional.
    let acp_agent = if paths.acp {
        Some(choose_acp_agent(&args, &status, io)?)
    } else {
        None
    };
    let adding_acp = acp_agent.is_some();
    let (default_model, small_model, utility_model) = if paths.models {
        choose_models(
            client,
            &args,
            [default_flag, small_flag, utility_flag],
            adding_acp,
            &mut status,
            io,
        )
        .await?
    } else {
        (None, None, None)
    };
    let mcp_servers = choose_mcp_servers(&args, &status, io)?;
    let default_agent = choose_default_agent(&args, &status, acp_agent.as_ref(), io)?;

    // The daemon fetches its official agents first when it has none.
    let result = client
        .run_onboarding(OnboardingRunParams {
            default_model_list: default_model,
            small_model_list: small_model,
            utility_model_list: utility_model,
            acp_agents: acp_agent.clone().map(|agent| vec![agent]),
            mcp_servers: (!mcp_servers.is_empty()).then_some(mcp_servers),
            default_agent,
            ..OnboardingRunParams::default()
        })
        .await
        .context("onboarding.run")?;
    for line in rules::run_summary(&result) {
        io.write_line(&line)?;
    }
    if let Some(agent) = &acp_agent
        && (args.acp_check
            || (!args.yes
                && confirm(
                    io,
                    "Start the ACP agent now to check it works? [Y/n] ",
                    true,
                )?))
    {
        check_acp_agent(client, &result.status, agent, io).await?;
    }
    if result.status.ready {
        io.write_line(
            "Cookie is set up. Run `cookie` to start a session on the default agent; `cookie agents list` shows the others.",
        )?;
    }
    Ok(())
}

/// Sets up the models path: a provider, then the three model fields.
async fn choose_models(
    client: &Client,
    args: &OnboardArgs,
    [default_flag, small_flag, utility_flag]: [Option<Vec<OnboardingModelEntry>>; 3],
    adding_acp: bool,
    status: &mut OnboardingStatus,
    io: &mut impl ConnectIo,
) -> anyhow::Result<ModelLists> {
    // 1. Provider.
    if status.missing.contains(&OnboardingRequirement::Provider) && !adding_acp {
        if args.yes {
            anyhow::bail!(
                "no model provider is connected: run `cookie onboard` without --yes to connect one"
            );
        }
        io.write_line("1. Provider: none is connected yet.")?;
        super::connect::connect_provider_flow(client, None, io).await?;
    } else if status.missing.contains(&OnboardingRequirement::Provider) {
        // With an ACP agent, the models are optional.
        if args.yes {
            return Ok((None, None, None));
        }
        io.write_line("1. Provider: none is connected yet.")?;
        super::connect::connect_provider_flow(client, None, io).await?;
    } else if !args.yes && confirm(io, "1. Connect another provider? [y/N] ", false)? {
        super::connect::connect_provider_flow(client, None, io).await?;
    }
    let models = client
        .runtime_snapshot()
        .await
        .context("runtime.snapshot.get")?
        .snapshot
        .models;
    *status = client.get_onboarding().await.context("onboarding.get")?;
    if models.is_empty() && adding_acp {
        return Ok((None, None, None));
    }

    // 2. to 4. The models.
    let required = !adding_acp;
    let default_model = choose_model(
        Slot::Default,
        default_flag,
        status,
        &models,
        args,
        required,
        io,
    )?;
    let small_model = choose_model(Slot::Small, small_flag, status, &models, args, required, io)?;
    let utility_model = choose_model(
        Slot::Utility,
        utility_flag,
        status,
        &models,
        args,
        required,
        io,
    )?;
    Ok((default_model, small_model, utility_model))
}

/// The three model fields' lists, each `None` to keep what is set.
type ModelLists = (
    Option<Vec<OnboardingModelEntry>>,
    Option<Vec<OnboardingModelEntry>>,
    Option<Vec<OnboardingModelEntry>>,
);

/// The ways to run agents: the flags decide when they name any; else the
/// user, asked unless `--yes` (then the models path).
fn choose_paths(
    args: &OnboardArgs,
    status: &OnboardingStatus,
    io: &mut impl ConnectIo,
) -> anyhow::Result<Paths> {
    let model_flags = !args.default_model.is_empty()
        || !args.small_model.is_empty()
        || !args.utility_model.is_empty();
    if args.acp.is_some() || model_flags {
        return Ok(Paths {
            models: model_flags || args.acp.is_none(),
            acp: args.acp.is_some(),
        });
    }
    if args.yes {
        return Ok(Paths {
            models: true,
            acp: false,
        });
    }
    let acp_done = status
        .paths
        .as_ref()
        .is_some_and(|paths| paths.acp.complete);
    let default = if acp_done { 2 } else { 1 };
    loop {
        let answer = io.read_public(&format!(
            "Run agents on: 1. model providers you connect, 2. an ACP agent (Claude Code, Codex, \
             ...) with its own login, 3. both [{default}]: "
        ))?;
        let choice = match answer.trim() {
            "" => default,
            text => text.parse::<u8>().unwrap_or(0),
        };
        match choice {
            1 => {
                return Ok(Paths {
                    models: true,
                    acp: false,
                });
            }
            2 => {
                return Ok(Paths {
                    models: false,
                    acp: true,
                });
            }
            3 => {
                return Ok(Paths {
                    models: true,
                    acp: true,
                });
            }
            _ => io.write_line("Answer 1, 2, or 3.")?,
        }
    }
}

/// The ACP agent to link, from the flags or the user.
fn choose_acp_agent(
    args: &OnboardArgs,
    status: &OnboardingStatus,
    io: &mut impl ConnectIo,
) -> anyhow::Result<OnboardingAcpAgentInput> {
    let place = match args.acp_where.as_deref() {
        Some("every") => Some(AcpPlace::EveryMachine),
        Some("local") => Some(AcpPlace::DaemonMachine),
        Some(other) => anyhow::bail!("--acp-where takes `every` or `local`, not `{other}`"),
        None => None,
    };
    let preset = match args.acp.as_deref() {
        Some("other") => None,
        Some(id) => Some(
            status
                .acp_presets
                .iter()
                .find(|preset| preset.id.as_str() == id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--acp `{id}` is no preset the daemon has ({}), nor `other`",
                        presets_label(status)
                    )
                })?,
        ),
        None => {
            io.write_line(
                "ACP agent: the program cookie launches, with its own login and models.",
            )?;
            for (index, preset) in status.acp_presets.iter().enumerate() {
                io.write_line(&format!("  {}. {}", index + 1, preset.name))?;
                io.write_line(&format!("     needs: {}", preset.needs))?;
            }
            let other = status.acp_presets.len() + 1;
            io.write_line(&format!("  {other}. Another ACP agent"))?;
            loop {
                let answer = io.read_public("ACP agent number [1]: ")?;
                let index = match answer.trim() {
                    "" => 1,
                    text => text.parse::<usize>().unwrap_or(0),
                };
                if index == other {
                    break None;
                }
                if let Some(preset) = index
                    .checked_sub(1)
                    .and_then(|index| status.acp_presets.get(index))
                {
                    break Some(preset);
                }
                io.write_line("Not a listed number.")?;
            }
        }
    };
    let place = match place {
        Some(place) => place,
        None if args.yes || args.acp.is_some() => AcpPlace::DaemonMachine,
        None => {
            io.write_line(&format!(
                "Where it runs: 1. {} (each needs it installed and logged in), 2. {}",
                AcpPlace::EveryMachine.label(),
                AcpPlace::DaemonMachine.label()
            ))?;
            let answer = io.read_public("Where [2]: ")?;
            if answer.trim() == "1" {
                AcpPlace::EveryMachine
            } else {
                AcpPlace::DaemonMachine
            }
        }
    };
    let mut agent = match preset {
        Some(preset) => {
            let mut agent = rules::acp_preset_input(preset, place);
            if let Some(command) = &args.acp_command {
                agent.command = rules::split_command_line(command).map_err(anyhow::Error::msg)?;
            }
            agent
        }
        None => {
            let id = match &args.acp_id {
                Some(id) => id.clone(),
                None if args.yes => anyhow::bail!("--acp other needs --acp-id"),
                None => io.read_public("Its ID (lowercase, such as `gemini`): ")?,
            };
            let command = match &args.acp_command {
                Some(command) => command.clone(),
                None if args.yes => anyhow::bail!("--acp other needs --acp-command"),
                None => io.read_public("The command line that starts it in ACP mode: ")?,
            };
            rules::other_acp_input(&id, &command, &args.acp_env, "", place)
                .map_err(anyhow::Error::msg)?
        }
    };
    if let Some(id) = &args.acp_id {
        agent.id = id
            .parse()
            .map_err(|_| anyhow::anyhow!("--acp-id `{id}` is not an agent ID"))?;
    }
    for line in &args.acp_env {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--acp-env `{line}` is not KEY=VALUE"))?;
        agent.env.insert(key.to_owned(), value.to_owned());
    }
    Ok(agent)
}

fn presets_label(status: &OnboardingStatus) -> String {
    if status.acp_presets.is_empty() {
        return "none yet: the official agents were not fetched".to_owned();
    }
    status
        .acp_presets
        .iter()
        .map(|preset| format!("`{}`", preset.id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The MCP servers to add, from `--mcp` or the user.
fn choose_mcp_servers(
    args: &OnboardArgs,
    status: &OnboardingStatus,
    io: &mut impl ConnectIo,
) -> anyhow::Result<Vec<OnboardingMcpServerInput>> {
    let runtime = args
        .mcp_runtime
        .as_deref()
        .map(|runtime| {
            runtime
                .parse::<RuntimeId>()
                .map_err(|_| anyhow::anyhow!("--mcp-runtime `{runtime}` is not a runtime ID"))
        })
        .transpose()?;
    let offered = rules::offered_mcp_presets(status);
    let chosen = if !args.mcp.is_empty() {
        args.mcp
            .iter()
            .map(|id| {
                offered
                    .iter()
                    .find(|preset| preset.id == *id)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("--mcp `{id}` is no MCP preset the daemon has"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    } else if args.yes || offered.is_empty() {
        return Ok(Vec::new());
    } else {
        io.write_line(
            "Tools (optional): MCP servers that need no login. Their calls ask you until allowed.",
        )?;
        for (index, preset) in offered.iter().enumerate() {
            let configured = if rules::mcp_preset_configured(status, preset, runtime.as_ref()) {
                " (set up)"
            } else {
                ""
            };
            io.write_line(&format!(
                "  {}. {} · {}{configured}: {}",
                index + 1,
                preset.name,
                preset.category,
                preset.description
            ))?;
        }
        let answer = io.read_public("Numbers to add, separated by commas (Enter skips): ")?;
        let mut chosen = Vec::new();
        for part in answer
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            match part
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| offered.get(index))
            {
                Some(preset) => chosen.push(*preset),
                None => io.write_line(&format!("`{part}` is not a listed number; skipped."))?,
            }
        }
        chosen
    };
    Ok(chosen
        .into_iter()
        .filter(|preset| !rules::mcp_preset_configured(status, preset, runtime.as_ref()))
        .map(|preset| rules::mcp_preset_input(preset, runtime.clone()))
        .collect())
}

/// `[agents] default` to set: the flag, else the ACP agent being linked
/// when the user says so; `None` keeps what is set.
fn choose_default_agent(
    args: &OnboardArgs,
    status: &OnboardingStatus,
    acp_agent: Option<&OnboardingAcpAgentInput>,
    io: &mut impl ConnectIo,
) -> anyhow::Result<Option<AgentId>> {
    if let Some(agent) = &args.default_agent {
        return agent
            .parse()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("--default-agent `{agent}` is not an agent ID"));
    }
    let Some(acp_agent) = acp_agent else {
        return Ok(None);
    };
    if args.yes {
        return Ok(None);
    }
    let current = status.default_agent.as_ref().map_or(
        "`primary`, else the first agent that can run",
        AgentId::as_str,
    );
    Ok(confirm(
        io,
        &format!(
            "Start new sessions with `{}`? Now: {current} [y/N] ",
            acp_agent.id
        ),
        false,
    )?
    .then(|| acp_agent.id.clone()))
}

/// Checks `agent` on each runtime that offers it now and prints what each
/// found.
async fn check_acp_agent(
    client: &Client,
    status: &OnboardingStatus,
    agent: &OnboardingAcpAgentInput,
    io: &mut impl ConnectIo,
) -> anyhow::Result<()> {
    let runtimes = status
        .paths
        .iter()
        .flat_map(|paths| &paths.acp.agents)
        .filter(|declared| declared.id == agent.id && declared.runtime == agent.runtime)
        .flat_map(|declared| &declared.runtimes)
        .filter(|runtime| runtime.available)
        .map(|runtime| runtime.runtime_id.clone())
        .collect::<Vec<_>>();
    if runtimes.is_empty() {
        io.write_line("No connected machine offers it yet; check it later with a session.")?;
    }
    for runtime_id in runtimes {
        io.write_line(&format!("Checking `{}` on `{runtime_id}`…", agent.id))?;
        let result = client
            .check_acp_agent(AcpCheckParams {
                runtime_id: runtime_id.clone(),
                agent: agent.id.clone(),
            })
            .await
            .context("acp.check")?;
        io.write_line(&format!("  {}", rules::acp_check_line(&result)))?;
    }
    Ok(())
}

fn describe_status(status: &OnboardingStatus, io: &mut impl ConnectIo) -> anyhow::Result<()> {
    let connected = if status.providers_connected.is_empty() {
        "none".to_owned()
    } else {
        status
            .providers_connected
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    io.write_line(&format!("Connected providers: {connected}"))?;
    for slot in [Slot::Default, Slot::Small, Slot::Utility] {
        io.write_line(&format!(
            "{}: {}",
            slot.field(),
            configured_label(status, slot.wire()).unwrap_or_else(|| "not set".into())
        ))?;
    }
    if status.ready {
        io.write_line("The daemon is set up; you can change any of this.")?;
    } else {
        let missing = status
            .missing
            .iter()
            .map(|requirement| rules::requirement_label(*requirement))
            .collect::<Vec<_>>()
            .join(", ");
        io.write_line(&format!("To finish: {missing}."))?;
    }
    Ok(())
}

fn confirm(io: &mut impl ConnectIo, prompt: &str, default: bool) -> anyhow::Result<bool> {
    let answer = io.read_public(prompt)?.trim().to_ascii_lowercase();
    Ok(match answer.as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

/// The list to set for `slot`: the flags, else the user's choices, else
/// nothing (keep what is set).
fn choose_model(
    slot: Slot,
    flag: Option<Vec<OnboardingModelEntry>>,
    status: &OnboardingStatus,
    models: &[AvailableModelDescriptor],
    args: &OnboardArgs,
    required: bool,
    io: &mut impl ConnectIo,
) -> anyhow::Result<Option<Vec<OnboardingModelEntry>>> {
    if flag.is_some() {
        return Ok(flag);
    }
    let missing = required
        && slot.required()
        && status
            .missing
            .contains(&OnboardingRequirement::DefaultModel);
    if args.yes {
        if missing {
            anyhow::bail!(
                "{} does not name an available model: pass --{}",
                slot.field(),
                slot.field().replace('_', "-")
            );
        }
        return Ok(None);
    }
    if models.is_empty() {
        anyhow::bail!("no connected provider offers a model yet");
    }
    let number = slot.number();
    let current = configured_label(status, slot.wire());
    let same_as = slot
        .earlier()
        .iter()
        .map(|(key, field)| format!(", `{key}` for the same as {}", rules::field_name(*field)))
        .collect::<String>();
    io.write_line(&format!(
        "{number}. {} ({}): choose by number, `/text` to search{same_as}{}.",
        slot.field(),
        slot.about(),
        match (&current, missing) {
            (Some(current), false) => format!(", Enter keeps {current}"),
            _ => String::new(),
        }
    ))?;
    let mut chosen: Vec<OnboardingModelEntry> = Vec::new();
    let mut query = String::new();
    loop {
        let groups = model_groups(models, &query);
        let listed = list_models(&groups, io)?;
        let prompt = if chosen.is_empty() {
            format!("{} number: ", slot.field())
        } else {
            format!(
                "Fallback {} after {} (number, Enter to finish): ",
                chosen.len() + 1,
                chosen
                    .iter()
                    .map(entry_label)
                    .collect::<Vec<_>>()
                    .join(", then ")
            )
        };
        let answer = io.read_public(&prompt)?;
        let answer = answer.trim();
        if answer.is_empty() {
            if !chosen.is_empty() {
                return Ok(Some(chosen));
            }
            if !missing && (current.is_some() || !slot.required()) {
                return Ok(None);
            }
            continue;
        }
        if let Some(search) = answer.strip_prefix('/') {
            query = search.to_owned();
            continue;
        }
        if let Some((_, field)) = slot
            .earlier()
            .iter()
            .find(|(key, _)| answer.len() == 1 && answer.starts_with(*key))
        {
            let entry = field_entry(*field);
            if chosen.contains(&entry) {
                io.write_line("Already in the list.")?;
            } else {
                chosen.push(entry);
            }
            continue;
        }
        let Some(model) = answer
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| listed.get(index).copied())
        else {
            io.write_line("Not a listed number.")?;
            continue;
        };
        if chosen
            .iter()
            .any(|entry| entry.model.as_ref() == Some(&model.key))
        {
            io.write_line("Already in the list.")?;
            continue;
        }
        let preselected = match slot {
            Slot::Default => default_model_variant(model),
            Slot::Small => small_model_variant(model),
            Slot::Utility => utility_model_variant(model),
        };
        let variant = choose_variant(model, preselected, io)?;
        chosen.push(model_entry(model, variant.as_ref()));
        if chosen.len() == cookie_agent_protocol::MAX_ONBOARDING_MODEL_ENTRIES {
            return Ok(Some(chosen));
        }
    }
}

fn list_models<'a>(
    groups: &[ModelGroup<'a>],
    io: &mut impl ConnectIo,
) -> anyhow::Result<Vec<&'a AvailableModelDescriptor>> {
    let mut listed = Vec::new();
    if groups.is_empty() {
        io.write_line("  No model matches; `/` alone lists them all.")?;
    }
    for group in groups {
        io.write_line(&format!("  {}", group.provider))?;
        for model in &group.models {
            listed.push(*model);
            io.write_line(&format!(
                "    {}. {} ({}) · {} tokens of context",
                listed.len(),
                model.display_name,
                model.key,
                model.capabilities.context_tokens
            ))?;
        }
    }
    Ok(listed)
}

fn choose_variant(
    model: &AvailableModelDescriptor,
    preselected: Option<VariantId>,
    io: &mut impl ConnectIo,
) -> anyhow::Result<Option<VariantId>> {
    let variants = rules::variants(model);
    if variants.is_empty() {
        return Ok(None);
    }
    io.write_line(&format!("  Variants of {}:", model.key))?;
    for (index, variant) in variants.iter().enumerate() {
        let mark = if Some(variant) == preselected.as_ref() {
            " (Enter)"
        } else {
            ""
        };
        io.write_line(&format!("    {}. {variant}{mark}", index + 1))?;
    }
    loop {
        let answer = io.read_public("  Variant number: ")?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(preselected);
        }
        if let Some(variant) = answer
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| variants.get(index))
        {
            return Ok(Some(variant.clone()));
        }
        io.write_line("Not a listed number.")?;
    }
}
