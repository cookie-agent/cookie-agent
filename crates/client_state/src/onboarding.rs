//! The onboarding rules every client shares
//! (`docs/site/specs/default-models-and-onboarding.md` section 9): the four
//! steps, which are done, the model lists the pickers show, the variant each
//! picker preselects, why Finish is disabled, and the summary of a run.

use cookie_agent_protocol::{
    AvailableModelDescriptor, ModelKey, OnboardingFileAction, OnboardingKitFile,
    OnboardingModelInput, OnboardingModelTable, OnboardingRequirement, OnboardingRunResult,
    OnboardingStatus, ProviderId, VariantId,
};

/// The steps of onboarding, in order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OnboardingStep {
    Provider,
    DefaultModel,
    SmallModel,
    Kit,
}

impl OnboardingStep {
    pub const ALL: [Self; 4] = [
        Self::Provider,
        Self::DefaultModel,
        Self::SmallModel,
        Self::Kit,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Provider => "Provider",
            Self::DefaultModel => "Default model",
            Self::SmallModel => "Small model",
            Self::Kit => "Starter kit",
        }
    }

    /// Whether the step is done for `status`; the kit step is optional and
    /// never required.
    #[must_use]
    pub fn done(self, status: &OnboardingStatus) -> bool {
        let missing = |requirement| status.missing.contains(&requirement);
        match self {
            Self::Provider => !missing(OnboardingRequirement::Provider),
            Self::DefaultModel => !missing(OnboardingRequirement::DefaultModel),
            Self::SmallModel => !missing(OnboardingRequirement::SmallModel),
            Self::Kit => false,
        }
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

/// The step to open on: the first required step not done, else the kit.
#[must_use]
pub fn first_step(status: &OnboardingStatus) -> OnboardingStep {
    OnboardingStep::ALL
        .into_iter()
        .find(|step| *step != OnboardingStep::Kit && !step.done(status))
        .unwrap_or(OnboardingStep::Kit)
}

/// What a requirement asks for, as a phrase.
#[must_use]
pub fn requirement_label(requirement: OnboardingRequirement) -> &'static str {
    match requirement {
        OnboardingRequirement::Provider => "connect a provider",
        OnboardingRequirement::DefaultModel => "choose a default model",
        OnboardingRequirement::SmallModel => "choose a small model",
        OnboardingRequirement::Unknown => "finish what the daemon still needs",
    }
}

/// Why Finish is disabled, given what the daemon still misses once the
/// client's own choices are applied: `None` when it can run.
///
/// `chosen_default` and `chosen_small` say whether the client has chosen a
/// model the run will set, which answers that requirement.
#[must_use]
pub fn finish_refusal(
    status: &OnboardingStatus,
    chosen_default: bool,
    chosen_small: bool,
) -> Option<String> {
    let remaining = status
        .missing
        .iter()
        .copied()
        .filter(|requirement| match requirement {
            OnboardingRequirement::DefaultModel => !chosen_default,
            OnboardingRequirement::SmallModel => !chosen_small,
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

/// The variant the small-model picker preselects: the first the model
/// lists. Models list variants in increasing effort, so it is the cheapest;
/// no variant name is assumed.
#[must_use]
pub fn small_model_variant(model: &AvailableModelDescriptor) -> Option<VariantId> {
    variants(model).into_iter().next()
}

/// A picker's choice as `onboarding.run` takes it: a bare key when the
/// variant is the model's default, else `{ model, variant }`.
#[must_use]
pub fn model_input(
    model: &AvailableModelDescriptor,
    variant: Option<&VariantId>,
) -> OnboardingModelInput {
    match variant {
        Some(variant) if model.default_variant.as_ref() != Some(variant) => {
            OnboardingModelInput::Table(OnboardingModelTable {
                model: model.key.clone(),
                variant: Some(cookie_agent_protocol::ConfiguredVariantRef::Named(
                    variant.clone(),
                )),
            })
        }
        _ => OnboardingModelInput::Key(model.key.clone()),
    }
}

/// Parses `key[:variant]`, as `cookie onboard --default-model` takes it.
pub fn parse_model_flag(text: &str) -> Result<OnboardingModelInput, String> {
    let (key, variant) = match text.rsplit_once(':') {
        Some((key, variant)) => (key, Some(variant)),
        None => (text, None),
    };
    let model = key
        .parse::<ModelKey>()
        .map_err(|error| format!("`{key}` is not a model key: {error}"))?;
    Ok(match variant {
        None => OnboardingModelInput::Key(model),
        Some(variant) => OnboardingModelInput::Table(OnboardingModelTable {
            model,
            variant: Some(
                serde_json::from_value(serde_json::Value::String(variant.to_owned()))
                    .map_err(|error| format!("`{variant}` is not a variant: {error}"))?,
            ),
        }),
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

/// A downloaded kit archive as `onboarding.preview` and `onboarding.run`
/// take it.
#[must_use]
pub fn archive_source(bytes: &[u8]) -> cookie_agent_protocol::OnboardingKitSource {
    use base64::Engine as _;
    cookie_agent_protocol::OnboardingKitSource::Archive {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
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

/// How a kit file's action reads in the kit step.
#[must_use]
pub fn kit_file_status(file: &OnboardingKitFile) -> &'static str {
    match file.action {
        OnboardingFileAction::Write => "new",
        OnboardingFileAction::Skip => "same as yours (skipped)",
        OnboardingFileAction::WriteNew => "differs → written as .new",
        OnboardingFileAction::Unknown => "changed by a newer daemon",
    }
}

/// The lines of a run's summary.
#[must_use]
pub fn run_summary(result: &OnboardingRunResult) -> Vec<String> {
    let mut lines = Vec::new();
    for setting in &result.settings {
        let value = match setting.as_str() {
            "default_model" => result.status.default_model.as_ref(),
            "small_model" => result.status.small_model.as_ref(),
            _ => None,
        };
        lines.push(match value {
            Some(value) => format!("Set {setting} to {}", model_label(value)),
            None => format!("Set {setting}"),
        });
    }
    lines.extend(result.written.iter().map(|path| format!("Wrote {path}")));
    lines.extend(
        result
            .written_new
            .iter()
            .map(|path| format!("Kept your {path}; the kit's version is {path}.new")),
    );
    lines.extend(
        result
            .skipped
            .iter()
            .map(|path| format!("Skipped {path} (same as yours)")),
    );
    if lines.is_empty() {
        lines.push("Nothing to change.".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AvailableModelDescriptor, OnboardingModelInput, OnboardingRequirement, OnboardingStatus,
    };

    use super::{
        OnboardingStep, finish_refusal, first_step, model_groups, model_input, parse_model_flag,
        small_model_variant,
    };

    fn status(missing: &[OnboardingRequirement]) -> OnboardingStatus {
        OnboardingStatus {
            ready: missing.is_empty(),
            missing: missing.to_vec(),
            providers_connected: Vec::new(),
            default_model: None,
            small_model: None,
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
    fn the_panel_opens_on_the_first_required_step_left() {
        use OnboardingRequirement::*;
        assert_eq!(
            first_step(&status(&[Provider, DefaultModel])),
            OnboardingStep::Provider
        );
        assert_eq!(
            first_step(&status(&[SmallModel])),
            OnboardingStep::SmallModel
        );
        assert_eq!(first_step(&status(&[])), OnboardingStep::Kit);
        assert_eq!(
            OnboardingStep::Provider.next(),
            Some(OnboardingStep::DefaultModel)
        );
        assert_eq!(OnboardingStep::Kit.next(), None);
        assert_eq!(OnboardingStep::Provider.previous(), None);
    }

    #[test]
    fn finish_says_what_is_left_after_the_client_s_choices() {
        use OnboardingRequirement::*;
        let missing = status(&[Provider, DefaultModel, SmallModel]);
        assert_eq!(
            finish_refusal(&missing, true, true).as_deref(),
            Some("Connect a provider first.")
        );
        assert_eq!(
            finish_refusal(&status(&[DefaultModel, SmallModel]), true, false).as_deref(),
            Some("Choose a small model first.")
        );
        assert_eq!(
            finish_refusal(&status(&[DefaultModel, SmallModel]), true, true),
            None
        );
    }

    #[test]
    fn the_small_model_preselects_the_first_listed_variant() {
        let listed = model("a/m", &["low", "medium", "high"], Some("medium"));
        assert_eq!(
            small_model_variant(&listed).map(|variant| variant.to_string()),
            Some("low".into())
        );
        assert_eq!(
            super::default_model_variant(&listed).map(|variant| variant.to_string()),
            Some("medium".into())
        );
        assert_eq!(small_model_variant(&model("a/plain", &[], None)), None);
    }

    #[test]
    fn a_choice_of_the_default_variant_is_a_bare_key() {
        let listed = model("a/m", &["low", "high"], Some("high"));
        let high = super::variants(&listed)[1].clone();
        assert!(matches!(
            model_input(&listed, Some(&high)),
            OnboardingModelInput::Key(_)
        ));
        let low = super::variants(&listed)[0].clone();
        assert!(matches!(
            model_input(&listed, Some(&low)),
            OnboardingModelInput::Table(_)
        ));
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
    fn model_flags_take_an_optional_variant() {
        assert!(matches!(
            parse_model_flag("openai/gpt-test").unwrap(),
            OnboardingModelInput::Key(_)
        ));
        let OnboardingModelInput::Table(table) = parse_model_flag("openai/gpt-test:high").unwrap()
        else {
            panic!("a table");
        };
        assert_eq!(table.model.to_string(), "openai/gpt-test");
        assert!(parse_model_flag("not a key").is_err());
    }
}
