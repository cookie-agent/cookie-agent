//! The onboarding rules every client shares
//! (`docs/site/specs/default-models-and-onboarding.md` section 9, and
//! `docs/site/specs/machines-on-demand.md` section 2.2): the four steps,
//! which are done, the model lists the pickers show, the variant each picker
//! preselects, the fallback lists a model step builds
//! (`docs/site/specs/model-fallback-lists.md` section 6), why Finish is
//! disabled, and the summary of a run.

use cookie_agent_protocol::{
    AcpCheckResult, AcpCheckStage, AcpPreset, AgentId, AvailableModelDescriptor,
    ConfiguredVariantRef, McpPreset, McpPresetPlace, ModelKey, OnboardingAcpAgentInput,
    OnboardingMcpServerInput, OnboardingModelEntry, OnboardingModelField, OnboardingRequirement,
    OnboardingRunResult, OnboardingStatus, ProviderId, RuntimeId, VariantId,
};

/// The steps of onboarding, in order. The small and utility models are
/// optional: agents that name them fall back to the next model of their
/// list.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OnboardingStep {
    Provider,
    DefaultModel,
    SmallModel,
    UtilityModel,
}

impl OnboardingStep {
    pub const ALL: [Self; 4] = [
        Self::Provider,
        Self::DefaultModel,
        Self::SmallModel,
        Self::UtilityModel,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Provider => "Provider",
            Self::DefaultModel => "Default model",
            Self::SmallModel => "Small model",
            Self::UtilityModel => "Utility model",
        }
    }

    /// Whether the step is done for `status`: a required step when the
    /// daemon no longer misses it, an optional one once its model is set.
    #[must_use]
    pub fn done(self, status: &OnboardingStatus) -> bool {
        let missing = |requirement| status.missing.contains(&requirement);
        match self {
            Self::Provider => !missing(OnboardingRequirement::Provider),
            Self::DefaultModel => !missing(OnboardingRequirement::DefaultModel),
            Self::SmallModel => status.small_model_list.is_some() || status.small_model.is_some(),
            Self::UtilityModel => {
                status.utility_model_list.is_some() || status.utility_model.is_some()
            }
        }
    }

    /// Whether the daemon needs the step done to be set up.
    #[must_use]
    pub const fn required(self) -> bool {
        matches!(self, Self::Provider | Self::DefaultModel)
    }

    #[must_use]
    pub fn next(self) -> Option<Self> {
        Self::ALL
            .iter()
            .position(|step| *step == self)
            .and_then(|index| Self::ALL.get(index + 1).copied())
    }

    #[must_use]
    pub fn previous(self) -> Option<Self> {
        Self::ALL
            .iter()
            .position(|step| *step == self)
            .and_then(|index| index.checked_sub(1))
            .map(|index| Self::ALL[index])
    }
}

/// The step to open on: the first step not done, else the last.
#[must_use]
pub fn first_step(status: &OnboardingStatus) -> OnboardingStep {
    OnboardingStep::ALL
        .into_iter()
        .find(|step| !step.done(status))
        .unwrap_or(OnboardingStep::UtilityModel)
}

/// What a requirement asks for, as a phrase.
#[must_use]
pub fn requirement_label(requirement: OnboardingRequirement) -> &'static str {
    match requirement {
        OnboardingRequirement::Provider => "connect a provider",
        OnboardingRequirement::DefaultModel => "choose a default model",
        OnboardingRequirement::SmallModel => "choose a small model",
        OnboardingRequirement::DefaultAgent => "fetch the official agents",
        OnboardingRequirement::Unknown => "finish what the daemon still needs",
    }
}

/// Why Finish is disabled, given what the daemon still misses once the
/// client's own choices are applied: `None` when it can run.
///
/// `chosen_default` says whether the client has chosen a default model the
/// run will set. The run fetches the official agents itself, so a missing
/// default agent never holds Finish back.
/// `adding_acp` says whether the client adds an ACP agent the run declares:
/// that completes the ACP path, so what only the models path needs does not
/// hold Finish back (`docs/site/specs/acp-onboarding.md` section 2).
#[must_use]
pub fn finish_refusal(
    status: &OnboardingStatus,
    chosen_default: bool,
    adding_acp: bool,
) -> Option<String> {
    let remaining = status
        .missing
        .iter()
        .copied()
        .filter(|requirement| match requirement {
            OnboardingRequirement::Provider => !adding_acp,
            OnboardingRequirement::DefaultModel => !chosen_default && !adding_acp,
            // Not required since client protocol 1.56.0; an older daemon
            // that misses it is told by the run.
            OnboardingRequirement::SmallModel | OnboardingRequirement::DefaultAgent => false,
            _ => true,
        })
        .map(requirement_label)
        .collect::<Vec<_>>();
    if remaining.is_empty() {
        return None;
    }
    let mut text = remaining.join(", ");
    if let Some(first) = text.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some(format!("{text} first."))
}

/// One group of the model pickers: a connected provider and its available
/// models matching the search, by display name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelGroup<'a> {
    pub provider: ProviderId,
    pub models: Vec<&'a AvailableModelDescriptor>,
}

/// The available models matching `query` (case-insensitive, over key and
/// display name), grouped by provider in provider order.
#[must_use]
pub fn model_groups<'a>(
    models: &'a [AvailableModelDescriptor],
    query: &str,
) -> Vec<ModelGroup<'a>> {
    let query = query.trim().to_lowercase();
    let mut groups: Vec<ModelGroup<'a>> = Vec::new();
    let mut matching = models
        .iter()
        .filter(|model| {
            query.is_empty()
                || model.key.to_string().to_lowercase().contains(&query)
                || model.display_name.to_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    matching.sort_by(|left, right| {
        (left.key.provider_id(), &left.display_name, &left.key).cmp(&(
            right.key.provider_id(),
            &right.display_name,
            &right.key,
        ))
    });
    for model in matching {
        let provider = model.key.provider_id();
        match groups.last_mut() {
            Some(group) if group.provider == provider => group.models.push(model),
            _ => groups.push(ModelGroup {
                provider,
                models: vec![model],
            }),
        }
    }
    groups
}

/// A model's variants in the order it lists them.
#[must_use]
pub fn variants(model: &AvailableModelDescriptor) -> Vec<VariantId> {
    if model.variant_order.is_empty() {
        model
            .variants
            .iter()
            .map(|variant| variant.id.clone())
            .collect()
    } else {
        model.variant_order.clone()
    }
}

/// The variant the default-model picker preselects: the model's own default.
#[must_use]
pub fn default_model_variant(model: &AvailableModelDescriptor) -> Option<VariantId> {
    model
        .default_variant
        .clone()
        .or_else(|| variants(model).into_iter().next())
}

/// The variant the small-model picker preselects: the model's own default,
/// as for the default model; subagents that carry out work run on it.
#[must_use]
pub fn small_model_variant(model: &AvailableModelDescriptor) -> Option<VariantId> {
    default_model_variant(model)
}

/// The variant the utility-model picker preselects: the first the model
/// lists. Models list variants in increasing effort, so it is the cheapest;
/// no variant name is assumed.
#[must_use]
pub fn utility_model_variant(model: &AvailableModelDescriptor) -> Option<VariantId> {
    variants(model).into_iter().next()
}

/// One entry a model step chose for its field's fallback list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelChoice {
    /// A model the picker chose, in a variant (`None`: the model has none).
    Model {
        model: ModelKey,
        variant: Option<VariantId>,
    },
    /// "Same as" another field: its models in this entry's place.
    Field(OnboardingModelField),
}

/// The field's name in `config.toml`: `default_model`, `small_model`, or
/// `utility_model`.
#[must_use]
pub const fn field_name(field: OnboardingModelField) -> &'static str {
    match field {
        OnboardingModelField::DefaultModel => "default_model",
        OnboardingModelField::SmallModel => "small_model",
        OnboardingModelField::UtilityModel => "utility_model",
        OnboardingModelField::Unknown => "another field",
    }
}

/// A picker's choice as a list entry: no variant when it is the model's
/// default, else the variant.
#[must_use]
pub fn model_entry(
    model: &AvailableModelDescriptor,
    variant: Option<&VariantId>,
) -> OnboardingModelEntry {
    OnboardingModelEntry {
        model: Some(model.key.clone()),
        field: None,
        variant: variant
            .filter(|variant| model.default_variant.as_ref() != Some(*variant))
            .map(|variant| ConfiguredVariantRef::Named(variant.clone())),
    }
}

/// The entry naming `field`.
#[must_use]
pub const fn field_entry(field: OnboardingModelField) -> OnboardingModelEntry {
    OnboardingModelEntry {
        model: None,
        field: Some(field),
        variant: None,
    }
}

/// `choices` as `onboarding.run` takes them, models looked up by
/// `descriptor` (a model no longer available is left out); `None` when
/// nothing is left.
pub fn choice_entries<'a>(
    choices: &[ModelChoice],
    descriptor: impl Fn(&ModelKey) -> Option<&'a AvailableModelDescriptor>,
) -> Option<Vec<OnboardingModelEntry>> {
    let entries = choices
        .iter()
        .filter_map(|choice| match choice {
            ModelChoice::Model { model, variant } => {
                descriptor(model).map(|descriptor| model_entry(descriptor, variant.as_ref()))
            }
            ModelChoice::Field(field) => Some(field_entry(*field)),
        })
        .collect::<Vec<_>>();
    (!entries.is_empty()).then_some(entries)
}

/// A choice as a short label: `provider/model (variant)`, or `same as
/// default_model`.
#[must_use]
pub fn choice_label(choice: &ModelChoice) -> String {
    match choice {
        ModelChoice::Model { model, variant } => match variant {
            Some(variant) => format!("{model} ({variant})"),
            None => model.to_string(),
        },
        ModelChoice::Field(field) => format!("same as {}", field_name(*field)),
    }
}

/// A list entry as a short label: `provider/model (variant)`, or `same as
/// default_model (variant)`.
#[must_use]
pub fn entry_label(entry: &OnboardingModelEntry) -> String {
    let base = match (&entry.model, entry.field) {
        (Some(model), _) => model.to_string(),
        (None, Some(field)) => format!("same as {}", field_name(field)),
        (None, None) => "?".into(),
    };
    match &entry.variant {
        Some(ConfiguredVariantRef::Named(variant)) => format!("{base} ({variant})"),
        Some(ConfiguredVariantRef::Base) => format!("{base} (base)"),
        None => base,
    }
}

/// A field's entries as one label, in fallback order: `a, then b`.
#[must_use]
pub fn list_label(entries: &[OnboardingModelEntry]) -> String {
    entries
        .iter()
        .map(entry_label)
        .collect::<Vec<_>>()
        .join(", then ")
}

/// How a status shows `field`: its list as configured, else the one model a
/// daemon older than client protocol 1.69.0 reports; `None` when unset.
#[must_use]
pub fn configured_label(status: &OnboardingStatus, field: OnboardingModelField) -> Option<String> {
    let (list, single) = match field {
        OnboardingModelField::DefaultModel => (&status.default_model_list, &status.default_model),
        OnboardingModelField::SmallModel => (&status.small_model_list, &status.small_model),
        OnboardingModelField::UtilityModel => (&status.utility_model_list, &status.utility_model),
        OnboardingModelField::Unknown => return None,
    };
    list.as_deref()
        .map(list_label)
        .or_else(|| single.as_ref().map(model_label))
}

/// Parses `key[:variant]`, or a field's name such as `default_model`, as
/// `cookie onboard --default-model` takes it. Model keys always hold a `/`,
/// so the two never meet.
pub fn parse_model_flag(text: &str) -> Result<OnboardingModelEntry, String> {
    for field in [
        OnboardingModelField::DefaultModel,
        OnboardingModelField::SmallModel,
        OnboardingModelField::UtilityModel,
    ] {
        let name = field_name(field);
        if text == name
            || text
                .strip_prefix("${")
                .and_then(|rest| rest.strip_suffix('}'))
                == Some(name)
        {
            return Ok(field_entry(field));
        }
    }
    let (key, variant) = match text.rsplit_once(':') {
        Some((key, variant)) => (key, Some(variant)),
        None => (text, None),
    };
    let model = key
        .parse::<ModelKey>()
        .map_err(|error| format!("`{key}` is not a model key or a model field: {error}"))?;
    Ok(OnboardingModelEntry {
        model: Some(model),
        field: None,
        variant: variant
            .map(|variant| {
                serde_json::from_value(serde_json::Value::String(variant.to_owned()))
                    .map_err(|error| format!("`{variant}` is not a variant: {error}"))
            })
            .transpose()?,
    })
}

/// Where a skill comes from, as clients mark it: `user`, `project`, or
/// `official <version>`.
#[must_use]
pub fn skill_origin(skill: &cookie_agent_protocol::SkillDescriptor) -> String {
    match skill.source {
        cookie_agent_protocol::SkillSource::User => "user".into(),
        cookie_agent_protocol::SkillSource::Project => "project".into(),
        cookie_agent_protocol::SkillSource::Official => match &skill.version {
            Some(version) => format!("official {version}"),
            None => "official".into(),
        },
    }
}

/// A model value as a short label: `provider/model` or
/// `provider/model (variant)`.
#[must_use]
pub fn model_label(model: &cookie_agent_protocol::OnboardingModel) -> String {
    match &model.variant {
        Some(cookie_agent_protocol::ConfiguredVariantRef::Named(variant)) => {
            format!("{} ({variant})", model.model)
        }
        Some(cookie_agent_protocol::ConfiguredVariantRef::Base) => {
            format!("{} (base)", model.model)
        }
        None => model.model.to_string(),
    }
}

/// The lines of a run's summary.
#[must_use]
pub fn run_summary(result: &OnboardingRunResult) -> Vec<String> {
    let mut lines = Vec::new();
    for setting in &result.settings {
        if let Some(line) = agents_and_tools_line(setting, &result.status) {
            lines.push(line);
            continue;
        }
        let field = match setting.as_str() {
            "default_model" => Some(OnboardingModelField::DefaultModel),
            "small_model" => Some(OnboardingModelField::SmallModel),
            "utility_model" => Some(OnboardingModelField::UtilityModel),
            _ => None,
        };
        lines.push(
            match field.and_then(|field| configured_label(&result.status, field)) {
                Some(value) => format!("Set {setting} to {value}"),
                None => format!("Set {setting}"),
            },
        );
    }
    if lines.is_empty() {
        lines.push("Nothing to change.".into());
    }
    let paths = result.status.paths.as_ref();
    if paths.is_some_and(|paths| paths.acp.complete && !paths.models.complete)
        && result.status.utility_model.is_none()
    {
        lines.push(
            "No model provider is connected: sessions are not titled automatically, and the \
             approval reviewer passes every question to you. Connect a provider for a utility \
             model any time."
                .into(),
        );
    }
    lines
}

/// A summary line for a setting of the ACP agents, MCP servers, or default
/// agent, as `onboarding.run` names them; `None` for the model fields.
fn agents_and_tools_line(setting: &str, status: &OnboardingStatus) -> Option<String> {
    let parts = setting.split('.').collect::<Vec<_>>();
    Some(match parts.as_slice() {
        ["acp", "agents", id] => format!("Added ACP agent `{id}` on every machine"),
        ["runtimes", runtime, "acp", "agents", id] => {
            format!("Added ACP agent `{id}` on {}", machine_label(runtime))
        }
        ["mcp", "servers", name] => format!("Added MCP server `{name}`"),
        ["runtimes", runtime, "mcp", "servers", name] => {
            format!("Added MCP server `{name}` on {}", machine_label(runtime))
        }
        ["agents", "default"] => format!(
            "New sessions start with `{}`",
            status
                .default_agent
                .as_ref()
                .map_or("the default agent", AgentId::as_str)
        ),
        _ if setting.starts_with("agents/") => format!("Wrote {setting}"),
        _ => return None,
    })
}

fn machine_label(runtime: &str) -> String {
    if runtime == "local" {
        "this daemon's machine".to_owned()
    } else {
        format!("`{runtime}`")
    }
}

/// Where an ACP agent onboarding links runs
/// (`docs/site/specs/acp-onboarding.md` section 4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpPlace {
    /// `[acp.agents.<id>]`: every runtime but `global`.
    EveryMachine,
    /// `[runtimes.local.acp.agents.<id>]`.
    DaemonMachine,
}

impl AcpPlace {
    pub const ALL: [Self; 2] = [Self::EveryMachine, Self::DaemonMachine];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::EveryMachine => "on every machine",
            Self::DaemonMachine => "on this daemon's machine only",
        }
    }

    /// The runtime whose table it goes in; `None` for every machine.
    #[must_use]
    pub fn runtime(self) -> Option<RuntimeId> {
        match self {
            Self::EveryMachine => None,
            Self::DaemonMachine => RuntimeId::new("local").ok(),
        }
    }
}

/// The declaration `preset` fills in, `place` deciding where it goes.
#[must_use]
pub fn acp_preset_input(preset: &AcpPreset, place: AcpPlace) -> OnboardingAcpAgentInput {
    OnboardingAcpAgentInput {
        id: preset.id.clone(),
        runtime: place.runtime(),
        command: preset.command.clone(),
        env: preset.env.clone(),
        description: Some(preset.description.clone()).filter(|text| !text.is_empty()),
        model: None,
        effort: None,
    }
}

/// An ACP agent the user describes: its ID, its command line (words
/// separated by spaces, a word with spaces in double quotes), and its
/// environment as `KEY=VALUE` lines.
pub fn other_acp_input(
    id: &str,
    command_line: &str,
    env: &[String],
    description: &str,
    place: AcpPlace,
) -> Result<OnboardingAcpAgentInput, String> {
    let id = AgentId::new(id.trim()).map_err(|_| {
        format!(
            "`{}` is not an agent ID: lowercase letters, digits, and hyphens",
            id.trim()
        )
    })?;
    let command = split_command_line(command_line)?;
    if command.is_empty() {
        return Err("give the command that starts the agent".into());
    }
    let mut variables = std::collections::BTreeMap::new();
    for line in env
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
    {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("`{line}` is not KEY=VALUE"))?;
        variables.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(OnboardingAcpAgentInput {
        id,
        runtime: place.runtime(),
        command,
        env: variables,
        description: Some(description.trim().to_owned()).filter(|text| !text.is_empty()),
        model: None,
        effort: None,
    })
}

/// `text` split into words at spaces, a word in double quotes kept whole.
pub fn split_command_line(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' | '\t' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            other => {
                word.push(other);
                started = true;
            }
        }
    }
    if quoted {
        return Err("a double quote is not closed".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// The command line `command` is, as [`split_command_line`] reads it back.
#[must_use]
pub fn join_command_line(command: &[String]) -> String {
    command
        .iter()
        .map(|word| {
            if word.is_empty() || word.contains(' ') {
                format!("\"{word}\"")
            } else {
                word.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The server `preset` adds, on `runtime` for a machine preset (the
/// daemon's own machine when `None`).
#[must_use]
pub fn mcp_preset_input(
    preset: &McpPreset,
    runtime: Option<RuntimeId>,
) -> OnboardingMcpServerInput {
    OnboardingMcpServerInput {
        name: preset.id.clone(),
        runtime: match preset.place {
            McpPresetPlace::Global => None,
            _ => runtime.or_else(|| RuntimeId::new("local").ok()),
        },
        server: preset.server.clone(),
    }
}

/// Whether `status` already has a server named like `preset` where it
/// would go.
#[must_use]
pub fn mcp_preset_configured(
    status: &OnboardingStatus,
    preset: &McpPreset,
    runtime: Option<&RuntimeId>,
) -> bool {
    let place = match preset.place {
        McpPresetPlace::Global => None,
        _ => runtime.cloned().or_else(|| RuntimeId::new("local").ok()),
    };
    status
        .mcp_servers
        .iter()
        .any(|server| server.name == preset.id && server.runtime == place)
}

/// The MCP presets the Tools step offers: those this build knows a place
/// for.
#[must_use]
pub fn offered_mcp_presets(status: &OnboardingStatus) -> Vec<&McpPreset> {
    status
        .mcp_presets
        .iter()
        .filter(|preset| preset.place != McpPresetPlace::Unknown)
        .collect()
}

/// One line saying what an `acp.check` found.
#[must_use]
pub fn acp_check_line(result: &AcpCheckResult) -> String {
    let agent = match (&result.agent_name, &result.agent_version) {
        (Some(name), Some(version)) => format!("{name} {version}"),
        (Some(name), None) => name.clone(),
        _ => "The agent".to_owned(),
    };
    if result.ok {
        let models = crate::acp::model_option(&result.options)
            .map(|option| format!("; models: {}", crate::acp::option_values_label(option)))
            .unwrap_or_default();
        return format!("{agent} works{models}");
    }
    let stage = match result.stage {
        AcpCheckStage::Declared => "is not available",
        AcpCheckStage::Start => "did not start",
        AcpCheckStage::Session => "started but did not open a session",
        AcpCheckStage::Ready | AcpCheckStage::Unknown => "failed",
    };
    match &result.error {
        Some(error) => format!("{agent} {stage}: {error}"),
        None => format!("{agent} {stage}"),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AvailableModelDescriptor, OnboardingModelField, OnboardingRequirement, OnboardingStatus,
    };

    use super::{
        ModelChoice, OnboardingStep, choice_entries, finish_refusal, first_step, model_entry,
        model_groups, parse_model_flag, small_model_variant, utility_model_variant,
    };

    fn status(missing: &[OnboardingRequirement]) -> OnboardingStatus {
        OnboardingStatus {
            ready: missing.is_empty(),
            missing: missing.to_vec(),
            providers_connected: Vec::new(),
            default_model: None,
            small_model: None,
            utility_model: None,
            default_model_list: None,
            small_model_list: None,
            utility_model_list: None,
            ..Default::default()
        }
    }

    fn model(key: &str, variants: &[&str], default: Option<&str>) -> AvailableModelDescriptor {
        let mut sorted = variants.to_vec();
        sorted.sort_unstable();
        serde_json::from_value(serde_json::json!({
            "key": key, "display_name": key,
            "capabilities": {
                "input": ["text"], "output": ["text"], "context_tokens": 8192,
                "output_tokens": 2048, "tool_calling": true, "parallel_tool_calls": true,
                "structured_output": false, "reasoning": true, "temperature": true,
                "top_p": true, "seed": true, "native_replay": "unsupported",
                "cancellation": "local_only", "media": {}
            },
            "input_media": [],
            "variants": sorted.iter().map(|id| serde_json::json!({
                "id": id, "display_name": id, "origin": "catalog"
            })).collect::<Vec<_>>(),
            "variant_order": variants,
            "default_variant": default,
        }))
        .expect("model")
    }

    #[test]
    fn the_panel_opens_on_the_first_step_left() {
        use OnboardingRequirement::*;
        assert_eq!(
            first_step(&status(&[Provider, DefaultModel])),
            OnboardingStep::Provider
        );
        // The optional models are left while unset.
        assert_eq!(first_step(&status(&[])), OnboardingStep::SmallModel);
        let chosen = |model: &str| {
            Some(cookie_agent_protocol::OnboardingModel {
                model: model.parse().unwrap(),
                variant: None,
            })
        };
        let all_set = OnboardingStatus {
            small_model: chosen("a/small"),
            utility_model: chosen("a/utility"),
            ..status(&[])
        };
        assert_eq!(first_step(&all_set), OnboardingStep::UtilityModel);
        assert_eq!(
            OnboardingStep::Provider.next(),
            Some(OnboardingStep::DefaultModel)
        );
        assert_eq!(OnboardingStep::UtilityModel.next(), None);
        assert_eq!(OnboardingStep::Provider.previous(), None);
        assert!(OnboardingStep::DefaultModel.required());
        assert!(!OnboardingStep::SmallModel.required());
        assert!(!OnboardingStep::UtilityModel.required());
    }

    #[test]
    fn finish_says_what_is_left_after_the_client_s_choices() {
        use OnboardingRequirement::*;
        assert_eq!(
            finish_refusal(&status(&[Provider, DefaultModel]), true, false).as_deref(),
            Some("Connect a provider first.")
        );
        assert_eq!(
            finish_refusal(&status(&[DefaultModel]), false, false).as_deref(),
            Some("Choose a default model first.")
        );
        // The run fetches the official agents; an older daemon's small
        // model is never required here.
        assert_eq!(
            finish_refusal(
                &status(&[DefaultModel, DefaultAgent, SmallModel]),
                true,
                false
            ),
            None
        );
        // An ACP agent the run adds completes the other path.
        assert_eq!(
            finish_refusal(&status(&[Provider, DefaultModel]), false, true),
            None
        );
    }

    #[test]
    fn command_lines_split_at_spaces_and_keep_quoted_words() {
        use super::{join_command_line, split_command_line};
        assert_eq!(
            split_command_line("npx -y  \"my agent\" --flag").unwrap(),
            ["npx", "-y", "my agent", "--flag"]
        );
        assert_eq!(split_command_line("  ").unwrap(), Vec::<String>::new());
        assert!(split_command_line("npx \"open").is_err());
        let words = vec!["npx".to_owned(), "my agent".to_owned(), String::new()];
        assert_eq!(
            split_command_line(&join_command_line(&words)).unwrap(),
            words
        );
    }

    #[test]
    fn another_acp_agent_needs_an_id_a_command_and_key_value_variables() {
        use super::{AcpPlace, other_acp_input};
        let input = other_acp_input(
            "gemini",
            "gemini --acp",
            &["MODE=v2".to_owned(), String::new()],
            "",
            AcpPlace::DaemonMachine,
        )
        .unwrap();
        assert_eq!(input.command, ["gemini", "--acp"]);
        assert_eq!(input.env.get("MODE").map(String::as_str), Some("v2"));
        assert_eq!(input.runtime.as_ref().map(|id| id.as_str()), Some("local"));
        assert_eq!(input.description, None);
        assert!(other_acp_input("Not An ID", "x", &[], "", AcpPlace::EveryMachine).is_err());
        assert!(other_acp_input("x", "", &[], "", AcpPlace::EveryMachine).is_err());
        assert!(
            other_acp_input("x", "x", &["NOEQUALS".into()], "", AcpPlace::EveryMachine).is_err()
        );
    }

    #[test]
    fn a_run_summary_names_agents_servers_and_the_default_agent() {
        let result = cookie_agent_protocol::OnboardingRunResult {
            written: Vec::new(),
            skipped: Vec::new(),
            written_new: Vec::new(),
            settings: vec![
                "acp.agents.claude".into(),
                "runtimes.lab.mcp.servers.playwright".into(),
                "agents.default".into(),
            ],
            status: OnboardingStatus {
                default_agent: Some("claude".parse().unwrap()),
                ..status(&[])
            },
        };
        let lines = super::run_summary(&result);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("`claude`"));
        assert!(lines[1].contains("`playwright`") && lines[1].contains("`lab`"));
        assert!(lines[2].contains("`claude`"));
    }

    #[test]
    fn the_utility_model_preselects_the_first_listed_variant() {
        let listed = model("a/m", &["low", "medium", "high"], Some("medium"));
        assert_eq!(
            utility_model_variant(&listed).map(|variant| variant.to_string()),
            Some("low".into())
        );
        assert_eq!(
            small_model_variant(&listed).map(|variant| variant.to_string()),
            Some("medium".into())
        );
        assert_eq!(
            super::default_model_variant(&listed).map(|variant| variant.to_string()),
            Some("medium".into())
        );
        assert_eq!(utility_model_variant(&model("a/plain", &[], None)), None);
    }

    #[test]
    fn a_choice_of_the_default_variant_carries_no_variant() {
        let listed = model("a/m", &["low", "high"], Some("high"));
        let high = super::variants(&listed)[1].clone();
        assert_eq!(model_entry(&listed, Some(&high)).variant, None);
        let low = super::variants(&listed)[0].clone();
        assert!(model_entry(&listed, Some(&low)).variant.is_some());
    }

    #[test]
    fn choices_become_entries_in_order_without_models_gone() {
        let listed = model("a/m", &[], None);
        let choices = [
            ModelChoice::Field(OnboardingModelField::DefaultModel),
            ModelChoice::Model {
                model: "a/gone".parse().unwrap(),
                variant: None,
            },
            ModelChoice::Model {
                model: "a/m".parse().unwrap(),
                variant: None,
            },
        ];
        let entries = choice_entries(&choices, |key| (key == &listed.key).then_some(&listed))
            .expect("entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].field, Some(OnboardingModelField::DefaultModel));
        assert_eq!(entries[1].model.as_ref(), Some(&listed.key));
        assert_eq!(choice_entries(&choices[1..2], |_| None), None);
    }

    #[test]
    fn models_group_by_provider_and_filter_by_search() {
        let models = [
            model("b/two", &[], None),
            model("a/one", &[], None),
            model("b/other", &[], None),
        ];
        let groups = model_groups(&models, "");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].provider.as_str(), "a");
        assert_eq!(groups[1].models.len(), 2);
        let searched = model_groups(&models, "OTH");
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].models[0].key.to_string(), "b/other");
    }

    #[test]
    fn official_skills_are_marked_with_their_version() {
        let skill =
            |source: &str, version: Option<&str>| -> cookie_agent_protocol::SkillDescriptor {
                serde_json::from_value(serde_json::json!({
                    "name": "tidy", "description": "Tidy.", "when_to_use": null,
                    "location": "/skills/tidy/SKILL.md", "source": source,
                    "precedence_winner": true, "permission_effect": "allow", "visible": true,
                    "user_invocable": true, "argument_hint": null, "version": version,
                }))
                .expect("skill")
            };
        assert_eq!(
            super::skill_origin(&skill("official", Some("1.2.0"))),
            "official 1.2.0"
        );
        assert_eq!(super::skill_origin(&skill("user", None)), "user");
        assert_eq!(super::skill_origin(&skill("project", None)), "project");
    }

    #[test]
    fn model_flags_take_an_optional_variant_or_name_a_field() {
        let plain = parse_model_flag("openai/gpt-test").unwrap();
        assert_eq!(plain.variant, None);
        let table = parse_model_flag("openai/gpt-test:high").unwrap();
        assert_eq!(table.model.unwrap().to_string(), "openai/gpt-test");
        assert!(table.variant.is_some());
        for field in ["default_model", "${default_model}"] {
            assert_eq!(
                parse_model_flag(field).unwrap().field,
                Some(OnboardingModelField::DefaultModel)
            );
        }
        assert!(parse_model_flag("not a key").is_err());
        assert!(parse_model_flag("parent_model").is_err());
    }
}
