use std::collections::BTreeMap;

use cookie_agent_identity::{ConfiguredModelDefault, VariantId};
use serde::Serialize;

use crate::{
    HeaderName, ProviderOptions, SafeStaticHeaderValue,
    authoring::{ManagedModelOverride, ReasoningBehavior, RequestDefaults, VariantDefinition},
    catalog::CatalogReasoningOption,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompiledVariantOrigin {
    ModelsDevEffort,
    ModelsDevToggle,
    ModelsDevBudgetTokens,
    Authored,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompiledVariant {
    pub id: VariantId,
    pub model_id: Option<crate::authoring::WireModelId>,
    pub display_name: String,
    pub defaults: RequestDefaults,
    pub options: ProviderOptions,
    pub reasoning: Option<ReasoningBehavior>,
    pub headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    pub origin: CompiledVariantOrigin,
}

type CompiledVariants = (
    BTreeMap<VariantId, CompiledVariant>,
    Vec<VariantId>,
    Option<VariantId>,
);

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum VariantCompileError {
    #[error("invalid_variant")]
    Invalid,
    #[error("variant_collision")]
    Collision,
    #[error("unknown_default_variant")]
    Default,
}

/// Largest generated thinking budget, matching the 32K output ceiling common
/// to reasoning clients.
const MAX_GENERATED_BUDGET: i64 = 31_999;

/// Visible output kept free when sizing generated thinking budgets. Requests
/// send the model's full output limit, and a thinking budget must fit inside
/// it (Anthropic Messages sends `max_tokens = visible + budget`; other wires
/// count thinking within the output limit), so a budget may use at most the
/// output limit less this reserve. 4,096 tokens still fits a final answer or a
/// sizeable tool call such as a file write after the model thinks; 1,024 would
/// truncate those. A 32K-output model (Claude Opus 4.1) keeps a 27,904-token
/// maximum budget.
pub(crate) const MIN_VISIBLE_OUTPUT_TOKENS: u64 = 4_096;

pub(crate) fn managed_variants(
    source: &[CatalogReasoningOption],
    output_limit: u64,
    supported: impl Fn(&ReasoningBehavior) -> bool,
    override_: Option<&ManagedModelOverride>,
    defaults: &RequestDefaults,
    options: &ProviderOptions,
) -> Result<CompiledVariants, VariantCompileError> {
    let budget_room = output_limit.saturating_sub(MIN_VISIBLE_OUTPUT_TOKENS);
    let (mut variants, mut order) = generated(
        source,
        i64::try_from(budget_room).unwrap_or(i64::MAX),
        supported,
    )?;
    for variant in variants.values_mut() {
        variant.defaults = defaults.clone();
        variant.options = options.clone();
    }
    if let Some(override_) = override_ {
        apply_directives(
            &mut variants,
            &mut order,
            &override_.variants,
            defaults,
            options,
        )?;
    }
    let default = resolve_default(
        override_.and_then(|value| value.default_variant.as_ref()),
        &variants,
    )?;
    Ok((variants, order, default))
}

pub(crate) fn custom_variants(
    directives: &BTreeMap<VariantId, VariantDefinition>,
    default: Option<&ConfiguredModelDefault>,
    defaults: &RequestDefaults,
    options: &ProviderOptions,
) -> Result<CompiledVariants, VariantCompileError> {
    let mut variants = BTreeMap::new();
    let mut order = Vec::new();
    apply_directives(&mut variants, &mut order, directives, defaults, options)?;
    let default = resolve_default(default, &variants)?;
    Ok((variants, order, default))
}

fn generated(
    source: &[CatalogReasoningOption],
    budget_room: i64,
    supported: impl Fn(&ReasoningBehavior) -> bool,
) -> Result<(BTreeMap<VariantId, CompiledVariant>, Vec<VariantId>), VariantCompileError> {
    let mut variants = BTreeMap::new();
    let mut order = Vec::new();
    for option in source {
        match option {
            CatalogReasoningOption::Effort { values } => {
                for value in values {
                    let (id, reasoning) = if let Some(value) = value.as_deref() {
                        let effort =
                            serde_json::from_value(serde_json::Value::String(value.to_owned()))
                                .map_err(|_| VariantCompileError::Invalid)?;
                        (value, ReasoningBehavior::Effort { value: effort })
                    } else {
                        ("off", ReasoningBehavior::Toggle { enabled: false })
                    };
                    insert_generated(
                        &mut variants,
                        &mut order,
                        id,
                        reasoning,
                        CompiledVariantOrigin::ModelsDevEffort,
                    )?;
                }
            }
            CatalogReasoningOption::Toggle => {
                for (id, enabled) in [("off", false), ("on", true)] {
                    insert_generated(
                        &mut variants,
                        &mut order,
                        id,
                        ReasoningBehavior::Toggle { enabled },
                        CompiledVariantOrigin::ModelsDevToggle,
                    )?;
                }
            }
            CatalogReasoningOption::BudgetTokens { min, max } => {
                for (id, value) in budget_levels(*min, *max, budget_room) {
                    insert_generated(
                        &mut variants,
                        &mut order,
                        id,
                        ReasoningBehavior::BudgetTokens { value },
                        CompiledVariantOrigin::ModelsDevBudgetTokens,
                    )?;
                }
            }
        }
    }
    suppress_redundant_generated_toggle_on(&mut variants, &mut order, supported);
    Ok((variants, order))
}

/// Generated budget variants: `budget-auto` for a dynamic (`-1`) minimum,
/// `budget-min` for a fixed minimum, then `budget-high` (half the usable
/// maximum, at least the minimum) and `budget-max`. The usable maximum is the
/// catalog maximum capped at 31,999 tokens and at the output limit less
/// [`MIN_VISIBLE_OUTPUT_TOKENS`];
/// levels that do not fit or repeat a lower level are skipped.
fn budget_levels(min: Option<i64>, max: Option<i64>, room: i64) -> Vec<(&'static str, i64)> {
    let mut levels = Vec::new();
    if min == Some(-1) {
        levels.push(("budget-auto", -1));
    }
    let floor = min.filter(|value| *value >= 0);
    if let Some(value) = floor.filter(|value| *value <= room) {
        levels.push(("budget-min", value));
    }
    let maximum = max
        .unwrap_or(MAX_GENERATED_BUDGET)
        .min(MAX_GENERATED_BUDGET)
        .min(room);
    let floor = floor.unwrap_or(0);
    if maximum > 0 && maximum >= floor {
        let high = ((maximum + 1) / 2).max(floor);
        for (id, value) in [("budget-high", high), ("budget-max", maximum)] {
            if levels.last().is_none_or(|(_, previous)| *previous < value) {
                levels.push((id, value));
            }
        }
    }
    levels
}

/// Drops a generated toggle `on` when a supported effort or budget level
/// already turns reasoning on. Levels the adapter cannot send do not count, so
/// a wire that only accepts the toggle keeps its `on`.
fn suppress_redundant_generated_toggle_on(
    variants: &mut BTreeMap<VariantId, CompiledVariant>,
    order: &mut Vec<VariantId>,
    supported: impl Fn(&ReasoningBehavior) -> bool,
) {
    let has_explicit_reasoning_level = variants.values().any(|variant| {
        variant.reasoning.as_ref().is_some_and(&supported)
            && matches!(
                (variant.origin, variant.reasoning.as_ref()),
                (
                    CompiledVariantOrigin::ModelsDevEffort,
                    Some(ReasoningBehavior::Effort { .. })
                ) | (
                    CompiledVariantOrigin::ModelsDevBudgetTokens,
                    Some(ReasoningBehavior::BudgetTokens { .. })
                )
            )
    });
    if !has_explicit_reasoning_level {
        return;
    }

    let toggle_on = variants.iter().find_map(|(id, variant)| {
        (variant.origin == CompiledVariantOrigin::ModelsDevToggle
            && matches!(
                variant.reasoning.as_ref(),
                Some(ReasoningBehavior::Toggle { enabled: true })
            ))
        .then(|| id.clone())
    });
    if let Some(id) = toggle_on {
        variants.remove(&id);
        order.retain(|candidate| candidate != &id);
    }
}

fn insert_generated(
    variants: &mut BTreeMap<VariantId, CompiledVariant>,
    order: &mut Vec<VariantId>,
    id: &str,
    reasoning: ReasoningBehavior,
    origin: CompiledVariantOrigin,
) -> Result<(), VariantCompileError> {
    let id = VariantId::new(id).map_err(|_| VariantCompileError::Invalid)?;
    let candidate = CompiledVariant {
        model_id: None,
        display_name: display_name(&id),
        id: id.clone(),
        defaults: RequestDefaults::default(),
        options: ProviderOptions::default(),
        reasoning: Some(reasoning),
        headers: BTreeMap::new(),
        origin,
    };
    if let Some(existing) = variants.get(&id) {
        if serde_json::to_value(&existing.reasoning).ok()
            != serde_json::to_value(&candidate.reasoning).ok()
        {
            return Err(VariantCompileError::Collision);
        }
        return Ok(());
    }
    order.push(id.clone());
    variants.insert(id, candidate);
    Ok(())
}

fn apply_directives(
    variants: &mut BTreeMap<VariantId, CompiledVariant>,
    order: &mut Vec<VariantId>,
    directives: &BTreeMap<VariantId, VariantDefinition>,
    defaults: &RequestDefaults,
    options: &ProviderOptions,
) -> Result<(), VariantCompileError> {
    for (id, directive) in directives {
        if directive.enabled == Some(false) {
            if directive.disabled_has_settings() {
                return Err(VariantCompileError::Invalid);
            }
            variants.remove(id);
            order.retain(|candidate| candidate != id);
            continue;
        }
        let variant = variants.entry(id.clone()).or_insert_with(|| {
            order.push(id.clone());
            CompiledVariant {
                model_id: None,
                id: id.clone(),
                display_name: display_name(id),
                defaults: defaults.clone(),
                options: options.clone(),
                reasoning: None,
                headers: BTreeMap::new(),
                origin: CompiledVariantOrigin::Authored,
            }
        });
        if let Some(name) = &directive.display_name {
            if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
                return Err(VariantCompileError::Invalid);
            }
            variant.display_name.clone_from(name);
        }
        if let Some(model_id) = &directive.model_id {
            variant.model_id = Some(model_id.clone());
        }
        if let Some(defaults) = &directive.generation_options {
            defaults.apply(&mut variant.defaults);
        }
        if let Some(options) = &directive.adaptor_options {
            options.apply(&mut variant.options);
        }
        if let Some(reasoning) = &directive.reasoning {
            variant.reasoning = Some(reasoning.clone());
        }
        // Keep deletion markers until global/provider/model headers are composed.
        variant.headers.extend(directive.headers().clone());
    }
    Ok(())
}

fn resolve_default(
    default: Option<&ConfiguredModelDefault>,
    variants: &BTreeMap<VariantId, CompiledVariant>,
) -> Result<Option<VariantId>, VariantCompileError> {
    match default {
        None | Some(ConfiguredModelDefault::Base) => Ok(None),
        Some(ConfiguredModelDefault::Named(id)) if variants.contains_key(id) => {
            Ok(Some(id.clone()))
        }
        Some(ConfiguredModelDefault::Named(_)) => Err(VariantCompileError::Default),
    }
}

fn display_name(id: &VariantId) -> String {
    id.as_str()
        .split(['-', '_', '.'])
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authoring::PartialRequestDefaults;

    fn managed_variants(
        source: &[CatalogReasoningOption],
        override_: Option<&ManagedModelOverride>,
    ) -> Result<CompiledVariants, VariantCompileError> {
        super::managed_variants(
            source,
            128_000,
            |_| true,
            override_,
            &RequestDefaults::default(),
            &ProviderOptions::default(),
        )
    }

    fn id(value: &str) -> VariantId {
        VariantId::new(value).unwrap()
    }

    fn names(order: &[VariantId]) -> Vec<&str> {
        order.iter().map(VariantId::as_str).collect()
    }

    fn override_with(
        variants: BTreeMap<VariantId, VariantDefinition>,
        default_variant: Option<ConfiguredModelDefault>,
    ) -> ManagedModelOverride {
        ManagedModelOverride {
            model_id: None,
            enabled: None,
            display_name: None,
            defaults: PartialRequestDefaults::default(),
            variants,
            default_variant,
            options: crate::AdaptorOptions::default(),
            pricing: None,
            compaction: crate::NativeCompactionConfig::Unsupported,
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn toggle_with_effort_suppresses_generated_on_and_resolves_default() {
        let source = [
            CatalogReasoningOption::Toggle,
            CatalogReasoningOption::Effort {
                values: vec![Some("low".into()), Some("high".into()), Some("max".into())],
            },
        ];
        let override_ = override_with(
            BTreeMap::new(),
            Some(ConfiguredModelDefault::Named(id("high"))),
        );

        let (variants, order, default) = managed_variants(&source, Some(&override_)).unwrap();

        assert_eq!(names(&order), ["off", "low", "high", "max"]);
        assert!(!variants.contains_key(&id("on")));
        assert_eq!(default, Some(id("high")));
    }

    #[test]
    fn toggle_only_preserves_generated_on_and_resolves_default() {
        let override_ = override_with(
            BTreeMap::new(),
            Some(ConfiguredModelDefault::Named(id("on"))),
        );

        let (variants, order, default) =
            managed_variants(&[CatalogReasoningOption::Toggle], Some(&override_)).unwrap();

        assert_eq!(names(&order), ["off", "on"]);
        assert!(variants.contains_key(&id("on")));
        assert_eq!(default, Some(id("on")));
    }

    #[test]
    fn effort_only_is_unchanged_and_resolves_default() {
        let override_ = override_with(
            BTreeMap::new(),
            Some(ConfiguredModelDefault::Named(id("high"))),
        );

        let (variants, order, default) = managed_variants(
            &[CatalogReasoningOption::Effort {
                values: vec![Some("low".into()), Some("high".into())],
            }],
            Some(&override_),
        )
        .unwrap();

        assert_eq!(names(&order), ["low", "high"]);
        assert_eq!(variants.len(), 2);
        assert_eq!(default, Some(id("high")));
    }

    #[test]
    fn toggle_with_budget_tokens_suppresses_generated_on() {
        let override_ = override_with(
            BTreeMap::new(),
            Some(ConfiguredModelDefault::Named(id("budget-max"))),
        );

        let (variants, order, default) = managed_variants(
            &[
                CatalogReasoningOption::Toggle,
                CatalogReasoningOption::BudgetTokens {
                    min: Some(1024),
                    max: Some(4096),
                },
            ],
            Some(&override_),
        )
        .unwrap();

        assert_eq!(
            names(&order),
            ["off", "budget-min", "budget-high", "budget-max"]
        );
        assert!(!variants.contains_key(&id("on")));
        assert_eq!(default, Some(id("budget-max")));
    }

    fn budgets(
        order: &[VariantId],
        variants: &BTreeMap<VariantId, CompiledVariant>,
    ) -> Vec<(String, i64)> {
        order
            .iter()
            .map(|id| match variants[id].reasoning {
                Some(ReasoningBehavior::BudgetTokens { value }) => (id.as_str().to_owned(), value),
                ref other => panic!("{id} is not a budget variant: {other:?}"),
            })
            .collect()
    }

    fn budget_variants(
        min: Option<i64>,
        max: Option<i64>,
        output_limit: u64,
    ) -> Vec<(String, i64)> {
        let (variants, order, _) = super::managed_variants(
            &[CatalogReasoningOption::BudgetTokens { min, max }],
            output_limit,
            |_| true,
            None,
            &RequestDefaults::default(),
            &ProviderOptions::default(),
        )
        .unwrap();
        budgets(&order, &variants)
    }

    fn levels(values: &[(&str, i64)]) -> Vec<(String, i64)> {
        values
            .iter()
            .map(|(id, value)| ((*id).to_owned(), *value))
            .collect()
    }

    #[test]
    fn budget_without_catalog_maximum_derives_high_and_max_levels() {
        // Claude Haiku 4.5: models.dev gives only `min: 1024`.
        assert_eq!(
            budget_variants(Some(1024), None, 64_000),
            levels(&[
                ("budget-min", 1024),
                ("budget-high", 16_000),
                ("budget-max", 31_999)
            ])
        );
    }

    #[test]
    fn budget_levels_leave_the_minimum_visible_output_free() {
        // Claude Opus 4.1: 32K output keeps 4,096 visible tokens.
        assert_eq!(
            budget_variants(Some(1024), None, 32_000),
            levels(&[
                ("budget-min", 1024),
                ("budget-high", 13_952),
                ("budget-max", 27_904)
            ])
        );
        // No room at all: no fixed budget fits.
        assert_eq!(budget_variants(Some(1024), None, 4_096), []);
        assert_eq!(
            budget_variants(None, Some(8000), 8_192),
            levels(&[("budget-high", 2048), ("budget-max", 4096)])
        );
    }

    #[test]
    fn budget_levels_respect_catalog_bounds_and_skip_duplicates() {
        assert_eq!(
            budget_variants(Some(128), Some(262_144), 400_000),
            levels(&[
                ("budget-min", 128),
                ("budget-high", 16_000),
                ("budget-max", 31_999)
            ])
        );
        assert_eq!(
            budget_variants(Some(1024), Some(1024), 64_000),
            levels(&[("budget-min", 1024)])
        );
        assert_eq!(
            budget_variants(Some(-1), Some(24_576), 65_536),
            levels(&[
                ("budget-auto", -1),
                ("budget-high", 12_288),
                ("budget-max", 24_576)
            ])
        );
    }

    #[test]
    fn managed_override_can_readd_on_after_generation_suppression() {
        let override_ = override_with(
            BTreeMap::from([(
                id("on"),
                VariantDefinition {
                    reasoning: Some(ReasoningBehavior::Toggle { enabled: true }),
                    ..VariantDefinition::default()
                },
            )]),
            Some(ConfiguredModelDefault::Named(id("on"))),
        );

        let (variants, order, default) = managed_variants(
            &[
                CatalogReasoningOption::Toggle,
                CatalogReasoningOption::Effort {
                    values: vec![Some("low".into())],
                },
            ],
            Some(&override_),
        )
        .unwrap();

        assert_eq!(names(&order), ["off", "low", "on"]);
        assert_eq!(variants[&id("on")].origin, CompiledVariantOrigin::Authored);
        assert_eq!(default, Some(id("on")));
    }
}
