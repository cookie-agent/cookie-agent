use std::collections::BTreeMap;

use cookie_agent_identity::{ConfiguredModelDefault, VariantId};
use serde::Serialize;

use crate::{
    ProviderOptions,
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
    pub origin: CompiledVariantOrigin,
}

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
) -> Result<(BTreeMap<VariantId, CompiledVariant>, Option<VariantId>), VariantCompileError> {
    let mut variants = generated(source, family)?;
    if let Some(override_) = override_ {
        apply_directives(&mut variants, &override_.variants)?;
    }
    let default = resolve_default(
        override_.and_then(|value| value.default_variant.as_ref()),
        &variants,
    )?;
    Ok((variants, default))
}

pub(crate) fn custom_variants(
    directives: &BTreeMap<VariantId, VariantDirective>,
    default: Option<&ConfiguredModelDefault>,
) -> Result<(BTreeMap<VariantId, CompiledVariant>, Option<VariantId>), VariantCompileError> {
    let mut variants = BTreeMap::new();
    apply_directives(&mut variants, directives)?;
    Ok((variants.clone(), resolve_default(default, &variants)?))
}

fn generated(
    source: &[CatalogReasoningOption],
    family: OvenAdapterFamily,
) -> Result<BTreeMap<VariantId, CompiledVariant>, VariantCompileError> {
    let mut variants = BTreeMap::new();
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
                        "budget-max",
                        ReasoningBehavior::BudgetTokens { value: *value },
                        CompiledVariantOrigin::ModelsDevBudgetTokens,
                        family,
                    )?;
                }
            }
        }
    }
    Ok(variants)
}

fn insert_generated(
    variants: &mut BTreeMap<VariantId, CompiledVariant>,
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
    variants.insert(id, candidate);
    Ok(())
}

fn apply_directives(
    variants: &mut BTreeMap<VariantId, CompiledVariant>,
    directives: &BTreeMap<VariantId, VariantDirective>,
) -> Result<(), VariantCompileError> {
    for (id, directive) in directives {
        match directive {
            VariantDirective::Add {
                display_name,
                defaults,
                options,
                reasoning,
            } => {
                if variants.contains_key(id) {
                    return Err(VariantCompileError::Collision);
                }
                variants.insert(
                    id.clone(),
                    authored(id, display_name, defaults, options, reasoning),
                );
            }
            VariantDirective::Replace {
                display_name,
                defaults,
                options,
                reasoning,
            } => {
                if !variants.contains_key(id) {
                    return Err(VariantCompileError::Invalid);
                }
                variants.insert(
                    id.clone(),
                    authored(id, display_name, defaults, options, reasoning),
                );
            }
            VariantDirective::Disable => {
                if variants.remove(id).is_none() {
                    return Err(VariantCompileError::Invalid);
                }
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
) -> CompiledVariant {
    CompiledVariant {
        id: id.clone(),
        display_name: authored_display_name
            .clone()
            .unwrap_or_else(|| display_name(id)),
        defaults: defaults.clone(),
        options: options.clone(),
        reasoning: reasoning.clone(),
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
