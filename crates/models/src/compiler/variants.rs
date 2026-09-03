use std::collections::BTreeMap;

use cookie_agent_identity::{ConfiguredModelDefault, VariantId};
use serde::Serialize;

use crate::{
    HeaderName, ProviderOptions, SafeStaticHeaderValue,
    adapters::OvenAdapterFamily,
    authoring::{ManagedModelOverride, ReasoningBehavior, RequestDefaults, VariantDirective},
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

pub(crate) fn managed_variants(
    source: &[CatalogReasoningOption],
    override_: Option<&ManagedModelOverride>,
    family: OvenAdapterFamily,
) -> Result<CompiledVariants, VariantCompileError> {
    let (mut variants, mut order) = generated(source, family)?;
    if let Some(override_) = override_ {
        apply_directives(&mut variants, &mut order, &override_.variants)?;
    }
    let default = resolve_default(
        override_.and_then(|value| value.default_variant.as_ref()),
        &variants,
    )?;
    Ok((variants, order, default))
}

pub(crate) fn custom_variants(
    directives: &BTreeMap<VariantId, VariantDirective>,
    default: Option<&ConfiguredModelDefault>,
) -> Result<CompiledVariants, VariantCompileError> {
    let mut variants = BTreeMap::new();
    let mut order = Vec::new();
    apply_directives(&mut variants, &mut order, directives)?;
    let default = resolve_default(default, &variants)?;
    Ok((variants, order, default))
}

fn generated(
    source: &[CatalogReasoningOption],
    family: OvenAdapterFamily,
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
                        family,
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
                        family,
                    )?;
                }
            }
            CatalogReasoningOption::BudgetTokens { min, max } => {
                if let Some(value) = min {
                    insert_generated(
                        &mut variants,
                        &mut order,
                        if *value == -1 {
                            "budget-auto"
                        } else {
                            "budget-min"
                        },
                        ReasoningBehavior::BudgetTokens { value: *value },
                        CompiledVariantOrigin::ModelsDevBudgetTokens,
                        family,
                    )?;
                }
                if let Some(value) = max {
                    insert_generated(
                        &mut variants,
                        &mut order,
                        "budget-max",
                        ReasoningBehavior::BudgetTokens { value: *value },
                        CompiledVariantOrigin::ModelsDevBudgetTokens,
                        family,
                    )?;
                }
            }
        }
    }
    suppress_redundant_generated_toggle_on(&mut variants, &mut order);
    Ok((variants, order))
}

fn suppress_redundant_generated_toggle_on(
    variants: &mut BTreeMap<VariantId, CompiledVariant>,
    order: &mut Vec<VariantId>,
) {
    let has_explicit_reasoning_level = variants.values().any(|variant| {
        matches!(
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
    family: OvenAdapterFamily,
) -> Result<(), VariantCompileError> {
    validate_reasoning(&reasoning, family)?;
    let id = VariantId::new(id).map_err(|_| VariantCompileError::Invalid)?;
    let candidate = CompiledVariant {
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
    directives: &BTreeMap<VariantId, VariantDirective>,
) -> Result<(), VariantCompileError> {
    for (id, directive) in directives {
        match directive {
            VariantDirective::Add {
                display_name,
                defaults,
                options,
                reasoning,
                headers,
            } => {
                if variants.contains_key(id) {
                    return Err(VariantCompileError::Collision);
                }
                variants.insert(
                    id.clone(),
                    authored(id, display_name, defaults, options, reasoning, headers),
                );
                order.push(id.clone());
            }
            VariantDirective::Replace {
                display_name,
                defaults,
                options,
                reasoning,
                headers,
            } => {
                if !variants.contains_key(id) {
                    return Err(VariantCompileError::Invalid);
                }
                variants.insert(
                    id.clone(),
                    authored(id, display_name, defaults, options, reasoning, headers),
                );
            }
            VariantDirective::Disable => {
                if variants.remove(id).is_none() {
                    return Err(VariantCompileError::Invalid);
                }
                order.retain(|candidate| candidate != id);
            }
        }
    }
    Ok(())
}

fn authored(
    id: &VariantId,
    authored_display_name: &Option<String>,
    defaults: &RequestDefaults,
    options: &ProviderOptions,
    reasoning: &Option<ReasoningBehavior>,
    headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
) -> CompiledVariant {
    CompiledVariant {
        id: id.clone(),
        display_name: authored_display_name
            .clone()
            .unwrap_or_else(|| display_name(id)),
        defaults: defaults.clone(),
        options: options.clone(),
        reasoning: reasoning.clone(),
        headers: headers.clone(),
        origin: CompiledVariantOrigin::Authored,
    }
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

fn validate_reasoning(
    reasoning: &ReasoningBehavior,
    family: OvenAdapterFamily,
) -> Result<(), VariantCompileError> {
    let valid = match reasoning {
        ReasoningBehavior::Effort { .. } => !matches!(family, OvenAdapterFamily::CohereV2Chat),
        ReasoningBehavior::Toggle { .. } | ReasoningBehavior::BudgetTokens { .. } => matches!(
            family,
            OvenAdapterFamily::Anthropic
                | OvenAdapterFamily::AnthropicCompatible
                | OvenAdapterFamily::AwsBedrockConverse
                | OvenAdapterFamily::GoogleGemini
                | OvenAdapterFamily::GoogleVertexGemini
                | OvenAdapterFamily::CohereV2Chat
        ),
    };
    if valid {
        Ok(())
    } else {
        Err(VariantCompileError::Invalid)
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

    fn id(value: &str) -> VariantId {
        VariantId::new(value).unwrap()
    }

    fn names(order: &[VariantId]) -> Vec<&str> {
        order.iter().map(VariantId::as_str).collect()
    }

    fn override_with(
        variants: BTreeMap<VariantId, VariantDirective>,
        default_variant: Option<ConfiguredModelDefault>,
    ) -> ManagedModelOverride {
        ManagedModelOverride {
            enabled: None,
            display_name: None,
            defaults: PartialRequestDefaults::default(),
            variants,
            default_variant,
            shape: None,
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

        let (variants, order, default) =
            managed_variants(&source, Some(&override_), OvenAdapterFamily::Anthropic).unwrap();

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

        let (variants, order, default) = managed_variants(
            &[CatalogReasoningOption::Toggle],
            Some(&override_),
            OvenAdapterFamily::Anthropic,
        )
        .unwrap();

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
            OvenAdapterFamily::Anthropic,
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
            OvenAdapterFamily::Anthropic,
        )
        .unwrap();

        assert_eq!(names(&order), ["off", "budget-min", "budget-max"]);
        assert!(!variants.contains_key(&id("on")));
        assert_eq!(default, Some(id("budget-max")));
    }

    #[test]
    fn managed_override_can_readd_on_after_generation_suppression() {
        let override_ = override_with(
            BTreeMap::from([(
                id("on"),
                VariantDirective::Add {
                    display_name: None,
                    defaults: RequestDefaults::default(),
                    options: ProviderOptions::default(),
                    reasoning: Some(ReasoningBehavior::Toggle { enabled: true }),
                    headers: BTreeMap::new(),
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
            OvenAdapterFamily::Anthropic,
        )
        .unwrap();

        assert_eq!(names(&order), ["off", "low", "on"]);
        assert_eq!(variants[&id("on")].origin, CompiledVariantOrigin::Authored);
        assert_eq!(default, Some(id("on")));
    }
}
