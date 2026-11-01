//! `cookie onboard`: set up a daemon, local or remote, through its
//! onboarding API (`docs/site/specs/default-models-and-onboarding.md`
//! section 7, and `docs/site/specs/machines-on-demand.md` section 2.2):
//! connect a provider when none is, choose `default_model`, and optionally
//! `small_model` and `utility_model`. The daemon fetches its official agents
//! as it finishes; there is no starter kit. It writes nothing itself; the
//! daemon does.

use anyhow::Context as _;
use clap::Args;
use cookie_agent_client_state::onboarding::{
    self as rules, ModelGroup, configured_label, default_model_variant, entry_label, field_entry,
    model_entry, model_groups, parse_model_flag, small_model_variant, utility_model_variant,
};
use cookie_agent_protocol::{
    AvailableModelDescriptor, OnboardingModelEntry, OnboardingModelField, OnboardingRequirement,
    OnboardingRunParams, OnboardingStatus, VariantId,
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

    // 1. Provider.
    if status.missing.contains(&OnboardingRequirement::Provider) {
        if args.yes {
            anyhow::bail!(
                "no model provider is connected: run `cookie onboard` without --yes to connect one"
            );
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
    status = client.get_onboarding().await.context("onboarding.get")?;

    // 2. to 4. The models.
    let default_model = choose_model(Slot::Default, default_flag, &status, &models, &args, io)?;
    let small_model = choose_model(Slot::Small, small_flag, &status, &models, &args, io)?;
    let utility_model = choose_model(Slot::Utility, utility_flag, &status, &models, &args, io)?;

    // The daemon fetches its official agents first when it has none.
    let result = client
        .run_onboarding(OnboardingRunParams {
            install_kit: false,
            source: None,
            default_model: None,
            small_model: None,
            utility_model: None,
            default_model_list: default_model,
            small_model_list: small_model,
            utility_model_list: utility_model,
        })
        .await
        .context("onboarding.run")?;
    for line in rules::run_summary(&result) {
        io.write_line(&line)?;
    }
    if result.status.ready {
        io.write_line(
            "Cookie is set up. Run `cookie` to start a session on the default agent; `cookie agents list` shows the others.",
        )?;
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
    io: &mut impl ConnectIo,
) -> anyhow::Result<Option<Vec<OnboardingModelEntry>>> {
    if flag.is_some() {
        return Ok(flag);
    }
    let missing = slot.required()
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
