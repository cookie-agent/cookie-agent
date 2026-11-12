//! The effort control (`docs/site/specs/web-polish.md`, section 6): one
//! control for a native model's variants and an ACP agent's effort option,
//! and the form it takes for what is on offer.

use cookie_agent_protocol::{AcpConfigOption, AvailableModelDescriptor, VariantOrigin};

/// One level the control offers: a native variant or an ACP effort value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Level {
    /// The variant's ID, or the ACP value the agent takes.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// What a control offers, in its order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Levels {
    /// What the control is called: "Effort", "Thinking budget", or
    /// "Variant" for the user's own.
    pub name: &'static str,
    pub levels: Vec<Level>,
    /// The level a selection without one runs, when known.
    pub default: Option<String>,
    /// Whether the levels are a scale from less to more thinking, which the
    /// control shows by warming its colour; the user's own variants are not
    /// known to be one.
    pub scale: bool,
    /// A models.dev toggle with its two variants, `off` and `on`, only.
    pub toggle: bool,
}

/// The form a control takes for its levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Nothing to choose: no levels, or one.
    Hidden,
    /// Two states of a toggle: a switch.
    Switch,
    /// The sliding pill, with each level's short name when `short`.
    Pill { short: bool },
    /// Too many levels for a pill: a list.
    List,
}

/// The most levels a pill shows with their whole names.
const WHOLE_NAMES: usize = 5;
/// The most levels a pill shows at all.
const MOST_IN_A_PILL: usize = 7;

impl Levels {
    /// The form the control takes: hidden with nothing to choose, a switch
    /// for a toggle, the pill up to seven levels (with short names past
    /// five), and a list past that.
    pub fn form(&self) -> Form {
        match self.levels.len() {
            0 | 1 => Form::Hidden,
            2 if self.toggle => Form::Switch,
            count if count <= WHOLE_NAMES => Form::Pill { short: false },
            count if count <= MOST_IN_A_PILL => Form::Pill { short: true },
            _ => Form::List,
        }
    }

    /// Where `id` is among the levels.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.levels.iter().position(|level| level.id == id)
    }

    /// The level to keep when the user moves from a model or agent whose
    /// current level is `current` to one offering these levels: the same
    /// level when it is offered here too, else this one's default.
    pub fn carried(&self, current: Option<&str>) -> Option<String> {
        current
            .filter(|current| self.position(current).is_some())
            .map(ToOwned::to_owned)
            .or_else(|| self.default.clone())
    }
}

/// A native model's variants, in its `variant_order` (any it leaves out
/// after, in the catalog's order).
pub fn native(model: &AvailableModelDescriptor) -> Levels {
    let ordered = model
        .variant_order
        .iter()
        .filter_map(|id| model.variants.iter().find(|variant| &variant.id == id));
    let rest = model
        .variants
        .iter()
        .filter(|variant| !model.variant_order.contains(&variant.id));
    let variants: Vec<_> = ordered.chain(rest).collect();
    let explicit = |origin: &VariantOrigin| *origin == VariantOrigin::Explicit;
    let scale = variants.iter().any(|variant| !explicit(&variant.origin));
    let budget = variants
        .iter()
        .any(|variant| variant.origin == VariantOrigin::ModelsDevBudgetTokens);
    let toggle = variants.len() == 2
        && variants
            .iter()
            .all(|variant| variant.origin == VariantOrigin::ModelsDevToggle);
    Levels {
        name: if !scale {
            "Variant"
        } else if budget {
            "Thinking budget"
        } else {
            "Effort"
        },
        levels: variants
            .iter()
            .map(|variant| Level {
                id: variant.id.to_string(),
                label: variant.display_name.clone(),
                description: (!explicit(&variant.origin))
                    .then(|| describe(variant.id.as_str()))
                    .flatten()
                    .map(ToOwned::to_owned),
            })
            .collect(),
        default: model.default_variant.as_ref().map(ToString::to_string),
        scale,
        toggle,
    }
}

/// An ACP agent's effort option's values, in the agent's order, with the
/// descriptions it gives them. The agent does not say which is its
/// default.
pub fn acp(option: &AcpConfigOption) -> Levels {
    Levels {
        name: "Effort",
        levels: option
            .values
            .iter()
            .map(|value| Level {
                id: value.value.clone(),
                label: value.name.clone(),
                description: value
                    .description
                    .clone()
                    .or_else(|| describe(&value.value.to_ascii_lowercase()).map(ToOwned::to_owned)),
            })
            .collect(),
        default: None,
        scale: true,
        toggle: false,
    }
}

/// What a level of thinking does, for the models.dev efforts, budgets, and
/// toggles cookie generates.
fn describe(id: &str) -> Option<&'static str> {
    Some(match id {
        "off" => "No extended thinking. Fastest and cheapest.",
        "on" => "Thinks before it answers.",
        "budget-auto" => "The model decides how long to think.",
        "minimal" => "Barely thinks; for trivial edits.",
        "low" => "Quick thinking for simple edits and questions.",
        "medium" => "Balanced for everyday work.",
        "high" => "Thinks longer on harder problems.",
        "xhigh" => "Extra-long thinking; noticeably slower.",
        "max" => "Thinks as long as it needs. Slowest and costliest.",
        _ => return None,
    })
}

/// A level's name in a pill too crowded for whole names: up to four
/// characters as it is, the usual abbreviations of the efforts, else its
/// first three characters.
pub fn short_label(label: &str) -> String {
    if label.chars().count() <= 4 {
        return label.to_owned();
    }
    match label.to_ascii_lowercase().as_str() {
        "minimal" => "Min".to_owned(),
        "medium" => "Med".to_owned(),
        "x-high" | "xhigh" | "extra high" => "XHi".to_owned(),
        "maximum" => "Max".to_owned(),
        _ => label.chars().take(3).collect(),
    }
}

/// The model chip's label: the model's display name and the level's, joined
/// by a middle dot (D2), or the model alone.
pub fn chip_label(model: &str, level: Option<&str>) -> String {
    match level {
        Some(level) => format!("{model} · {level}"),
        None => model.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AcpConfigValue, AvailableVariantDescriptor, ModelCapabilities, VariantId,
    };

    use super::*;

    fn level(id: &str) -> Level {
        Level {
            id: id.to_owned(),
            label: id.to_owned(),
            description: None,
        }
    }

    fn levels(count: usize, toggle: bool) -> Levels {
        Levels {
            name: "Effort",
            levels: (0..count)
                .map(|index| level(&format!("l{index}")))
                .collect(),
            default: None,
            scale: true,
            toggle,
        }
    }

    #[test]
    fn the_form_follows_how_many_levels_there_are() {
        assert_eq!(levels(0, false).form(), Form::Hidden);
        assert_eq!(levels(1, false).form(), Form::Hidden);
        assert_eq!(levels(2, true).form(), Form::Switch);
        assert_eq!(levels(2, false).form(), Form::Pill { short: false });
        assert_eq!(levels(5, false).form(), Form::Pill { short: false });
        assert_eq!(levels(6, false).form(), Form::Pill { short: true });
        assert_eq!(levels(7, false).form(), Form::Pill { short: true });
        assert_eq!(levels(8, false).form(), Form::List);
    }

    #[test]
    fn a_level_carries_over_when_offered_else_the_default_takes_over() {
        let mut offered = levels(3, false);
        offered.default = Some("l0".to_owned());
        assert_eq!(offered.carried(Some("l2")).as_deref(), Some("l2"));
        assert_eq!(offered.carried(Some("max")).as_deref(), Some("l0"));
        assert_eq!(offered.carried(None).as_deref(), Some("l0"));
        offered.default = None;
        assert_eq!(offered.carried(Some("max")), None);
    }

    #[test]
    fn short_labels_keep_short_names_and_abbreviate_the_rest() {
        assert_eq!(short_label("Low"), "Low");
        assert_eq!(short_label("High"), "High");
        assert_eq!(short_label("Minimal"), "Min");
        assert_eq!(short_label("Medium"), "Med");
        assert_eq!(short_label("X-High"), "XHi");
        assert_eq!(short_label("standard"), "sta");
    }

    #[test]
    fn the_chip_joins_the_model_and_its_level() {
        assert_eq!(chip_label("Opus 5.5", Some("High")), "Opus 5.5 · High");
        assert_eq!(chip_label("GPT-6", None), "GPT-6");
    }

    fn model(
        variants: &[(&str, VariantOrigin)],
        order: &[&str],
        default: Option<&str>,
    ) -> AvailableModelDescriptor {
        let id = |id: &str| VariantId::new(id.to_owned()).expect("a variant ID");
        AvailableModelDescriptor {
            key: "openai/gpt-6".parse().expect("a model key"),
            display_name: "GPT-6".to_owned(),
            capabilities: ModelCapabilities {
                input: [cookie_agent_protocol::Modality::Text]
                    .into_iter()
                    .collect(),
                output: [cookie_agent_protocol::Modality::Text]
                    .into_iter()
                    .collect(),
                context_tokens: 4096,
                output_tokens: 1024,
                tool_calling: true,
                parallel_tool_calls: true,
                structured_output: false,
                reasoning: true,
                temperature: true,
                top_p: true,
                seed: true,
                native_replay: cookie_agent_protocol::ReplayCapability::Unsupported,
                cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
                media: Default::default(),
            },
            input_media: Vec::new(),
            variants: variants
                .iter()
                .map(|(name, origin)| AvailableVariantDescriptor {
                    id: id(name),
                    display_name: name.to_uppercase(),
                    origin: *origin,
                })
                .collect(),
            variant_order: order.iter().map(|name| id(name)).collect(),
            default_variant: default.map(id),
        }
    }

    #[test]
    fn a_native_model_offers_its_variants_in_its_order() {
        let effort = VariantOrigin::ModelsDevEffort;
        let offered = native(&model(
            &[
                ("high", effort),
                ("low", effort),
                ("off", VariantOrigin::ModelsDevToggle),
            ],
            &["off", "low", "high"],
            Some("low"),
        ));
        let ids: Vec<_> = offered
            .levels
            .iter()
            .map(|level| level.id.as_str())
            .collect();
        assert_eq!(ids, ["off", "low", "high"]);
        assert_eq!(offered.default.as_deref(), Some("low"));
        assert!(offered.scale && !offered.toggle);
        assert_eq!(offered.name, "Effort");
        assert!(
            offered
                .levels
                .iter()
                .all(|level| level.description.is_some())
        );
    }

    #[test]
    fn a_native_toggle_is_a_switch_and_own_variants_are_no_scale() {
        let toggle = VariantOrigin::ModelsDevToggle;
        let switch = native(&model(
            &[("off", toggle), ("on", toggle)],
            &["off", "on"],
            Some("on"),
        ));
        assert_eq!(switch.form(), Form::Switch);
        let own = native(&model(
            &[
                ("fast", VariantOrigin::Explicit),
                ("precise", VariantOrigin::Explicit),
            ],
            &["fast", "precise"],
            None,
        ));
        assert!(!own.scale);
        assert_eq!(own.name, "Variant");
        assert!(own.levels.iter().all(|level| level.description.is_none()));
        let budgets = native(&model(
            &[
                ("low", VariantOrigin::ModelsDevBudgetTokens),
                ("high", VariantOrigin::ModelsDevBudgetTokens),
            ],
            &["low", "high"],
            None,
        ));
        assert_eq!(budgets.name, "Thinking budget");
    }

    #[test]
    fn an_acp_agent_offers_its_effort_values_in_its_order() {
        let option = AcpConfigOption {
            id: "effort".to_owned(),
            name: "Effort".to_owned(),
            kind: Default::default(),
            description: None,
            category: Some("thought_level".to_owned()),
            current_value: "high".to_owned(),
            values: ["low", "high", "max"]
                .iter()
                .map(|value| AcpConfigValue {
                    value: (*value).to_owned(),
                    name: value.to_uppercase(),
                    description: (*value == "max").then(|| "The agent's own words.".to_owned()),
                })
                .collect(),
        };
        let offered = acp(&option);
        let ids: Vec<_> = offered
            .levels
            .iter()
            .map(|level| level.id.as_str())
            .collect();
        assert_eq!(ids, ["low", "high", "max"]);
        assert_eq!(
            offered.levels[2].description.as_deref(),
            Some("The agent's own words.")
        );
        assert!(offered.levels[0].description.is_some());
        assert_eq!(offered.default, None);
    }
}
