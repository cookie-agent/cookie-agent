use std::collections::BTreeMap;

use cookie_agent_config::{ModelPricing, PicoUsdPerMillion, PricingConfig};
use cookie_agent_models::catalog::{CatalogModelCost, CatalogModelCostRates};
use cookie_agent_protocol::{
    ModelKey, ModelUsageRollup, ResolvedModelRef, Usage, UsageCostProvenance, UsageRollup,
};

const PICO_USD_PER_USD: u128 = 1_000_000_000_000;
const TOKENS_PER_MILLION: u128 = 1_000_000;

pub(crate) fn record_stamped(
    rollup: &mut UsageRollup,
    model: &ResolvedModelRef,
    usage: &Usage,
    estimated_cost_pico_usd: Option<u64>,
) {
    record_with_provenance(
        rollup,
        model,
        usage,
        UsageCostProvenance::Stamped(estimated_cost_pico_usd),
    );
}

fn record_with_provenance(
    rollup: &mut UsageRollup,
    model: &ResolvedModelRef,
    usage: &Usage,
    cost_provenance: UsageCostProvenance,
) {
    let model_key = ModelKey::new(model.provider_id.clone(), model.model_id.clone())
        .expect("resolved model identity is valid");
    let model_rollup = rollup.by_model.entry(model_key).or_default();
    record_model(model_rollup, usage, cost_provenance);
    add_observed(
        &mut rollup.input_tokens,
        usage.input_tokens,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.output_tokens,
        usage.output_tokens,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.reasoning_tokens,
        usage.output_tokens_reasoning,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.cache_read_tokens,
        usage.input_tokens_cache_read,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.cache_write_tokens,
        usage.input_tokens_cache_write,
        &mut rollup.arithmetic_overflow,
    );
    checked_increment(
        &mut rollup.request_count,
        1,
        &mut rollup.arithmetic_overflow,
    );
}

fn record_model(
    rollup: &mut ModelUsageRollup,
    usage: &Usage,
    cost_provenance: UsageCostProvenance,
) {
    rollup.observations.push(usage.clone());
    rollup.cost_provenance.push(cost_provenance);
    add_observed(
        &mut rollup.input_tokens,
        usage.input_tokens,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.output_tokens,
        usage.output_tokens,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.reasoning_tokens,
        usage.output_tokens_reasoning,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.cache_read_tokens,
        usage.input_tokens_cache_read,
        &mut rollup.arithmetic_overflow,
    );
    add_observed(
        &mut rollup.cache_write_tokens,
        usage.input_tokens_cache_write,
        &mut rollup.arithmetic_overflow,
    );
    checked_increment(
        &mut rollup.request_count,
        1,
        &mut rollup.arithmetic_overflow,
    );
}

fn add_observed(total: &mut u64, value: Option<u64>, overflow: &mut bool) {
    if let Some(value) = value {
        checked_increment(total, value, overflow);
    }
}

fn checked_increment(total: &mut u64, value: u64, overflow: &mut bool) {
    if let Some(sum) = total.checked_add(value) {
        *total = sum;
    } else {
        *overflow = true;
    }
}

pub(crate) fn merge(target: &mut UsageRollup, source: &UsageRollup) {
    target.arithmetic_overflow |= source.arithmetic_overflow;
    checked_increment(
        &mut target.input_tokens,
        source.input_tokens,
        &mut target.arithmetic_overflow,
    );
    checked_increment(
        &mut target.output_tokens,
        source.output_tokens,
        &mut target.arithmetic_overflow,
    );
    checked_increment(
        &mut target.reasoning_tokens,
        source.reasoning_tokens,
        &mut target.arithmetic_overflow,
    );
    checked_increment(
        &mut target.cache_read_tokens,
        source.cache_read_tokens,
        &mut target.arithmetic_overflow,
    );
    checked_increment(
        &mut target.cache_write_tokens,
        source.cache_write_tokens,
        &mut target.arithmetic_overflow,
    );
    checked_increment(
        &mut target.request_count,
        source.request_count,
        &mut target.arithmetic_overflow,
    );
    for (model, source_model) in &source.by_model {
        let target_model = target.by_model.entry(model.clone()).or_default();
        target_model.arithmetic_overflow |= source_model.arithmetic_overflow;
        target_model
            .observations
            .extend(source_model.observations.iter().cloned());
        target_model
            .cost_provenance
            .extend(source_model.cost_provenance.iter().copied());
        checked_increment(
            &mut target_model.input_tokens,
            source_model.input_tokens,
            &mut target_model.arithmetic_overflow,
        );
        checked_increment(
            &mut target_model.output_tokens,
            source_model.output_tokens,
            &mut target_model.arithmetic_overflow,
        );
        checked_increment(
            &mut target_model.reasoning_tokens,
            source_model.reasoning_tokens,
            &mut target_model.arithmetic_overflow,
        );
        checked_increment(
            &mut target_model.cache_read_tokens,
            source_model.cache_read_tokens,
            &mut target_model.arithmetic_overflow,
        );
        checked_increment(
            &mut target_model.cache_write_tokens,
            source_model.cache_write_tokens,
            &mut target_model.arithmetic_overflow,
        );
        checked_increment(
            &mut target_model.request_count,
            source_model.request_count,
            &mut target_model.arithmetic_overflow,
        );
    }
}

pub(crate) fn with_pricing(
    mut rollup: UsageRollup,
    _pricing: &PricingConfig,
    _catalog: &BTreeMap<ModelKey, CatalogModelCost>,
) -> UsageRollup {
    rollup.cache_hit_rate = (!rollup.arithmetic_overflow)
        .then(|| {
            hit_rate(
                rollup
                    .by_model
                    .values()
                    .flat_map(|model| &model.observations),
            )
        })
        .flatten();
    let mut total_cost_numerator = 0_u128;
    let mut all_priced = rollup.request_count > 0 && !rollup.arithmetic_overflow;
    for usage in rollup.by_model.values_mut() {
        usage.cache_hit_rate = (!usage.arithmetic_overflow)
            .then(|| hit_rate(usage.observations.iter()))
            .flatten();
        let cost = if usage.arithmetic_overflow
            || usage.observations.len()
                != usize::try_from(usage.request_count).unwrap_or(usize::MAX)
            || usage.cost_provenance.len() != usage.observations.len()
        {
            None
        } else {
            observations_cost(&usage.cost_provenance)
        };
        usage.estimated_cost_usd = cost.map(cost_numerator_to_usd);
        if let Some(cost) = cost {
            if let Some(total) = total_cost_numerator.checked_add(cost) {
                total_cost_numerator = total;
            } else {
                all_priced = false;
            }
        } else {
            all_priced = false;
        }
    }
    rollup.estimated_cost_usd = all_priced.then(|| cost_numerator_to_usd(total_cost_numerator));
    rollup
}

pub(crate) fn estimated_cost_pico_usd(
    model: &ResolvedModelRef,
    usage: &Usage,
    pricing: &PricingConfig,
    catalog: &BTreeMap<ModelKey, CatalogModelCost>,
) -> Option<u64> {
    let model_key = ModelKey::new(model.provider_id.clone(), model.model_id.clone()).ok()?;
    let rates = if let Some(rates) = pricing.models.get(&model_key) {
        *rates
    } else {
        let input_tokens = usage.input_tokens?;
        catalog_rates(catalog.get(&model_key)?.rates_for_input(input_tokens))
    };
    let numerator = request_cost(usage, rates)?;
    let rounded_pico_usd = numerator
        .checked_add(TOKENS_PER_MILLION / 2)?
        .checked_div(TOKENS_PER_MILLION)?;
    u64::try_from(rounded_pico_usd).ok()
}

fn hit_rate<'a>(observations: impl Iterator<Item = &'a Usage>) -> Option<f64> {
    let mut input = 0_u64;
    let mut cache_read = 0_u64;
    let mut observed = false;
    for usage in observations {
        observed = true;
        input = input.checked_add(usage.input_tokens?)?;
        cache_read = cache_read.checked_add(usage.input_tokens_cache_read?)?;
    }
    if !observed {
        None
    } else if input == 0 {
        Some(0.0)
    } else {
        Some((cache_read.min(input) as f64) / (input as f64))
    }
}

fn observations_cost(cost_provenance: &[UsageCostProvenance]) -> Option<u128> {
    let mut total = 0_u128;
    for provenance in cost_provenance {
        let cost = match provenance {
            UsageCostProvenance::Stamped(Some(pico_usd)) => {
                u128::from(*pico_usd).checked_mul(TOKENS_PER_MILLION)?
            }
            UsageCostProvenance::Stamped(None) => return None,
        };
        total = total.checked_add(cost)?;
    }
    Some(total)
}

fn request_cost(usage: &Usage, rates: ModelPricing) -> Option<u128> {
    let input = usage.input_tokens?;
    let output = usage.output_tokens?;
    let input_cost = split_cost(
        input,
        rates.input_per_million_usd,
        [
            (
                usage.input_tokens_cache_read,
                rates
                    .cache_read_per_million_usd
                    .or(rates.input_per_million_usd),
            ),
            (
                usage.input_tokens_cache_write,
                rates
                    .cache_write_per_million_usd
                    .or(rates.input_per_million_usd),
            ),
        ],
    )?;
    let output_cost = split_cost(
        output,
        rates.output_per_million_usd,
        [(
            usage.output_tokens_reasoning,
            rates
                .reasoning_per_million_usd
                .or(rates.output_per_million_usd),
        )],
    )?;
    input_cost.checked_add(output_cost)
}

fn split_cost<const N: usize>(
    inclusive_tokens: u64,
    base_rate: Option<PicoUsdPerMillion>,
    categories: [(Option<u64>, Option<PicoUsdPerMillion>); N],
) -> Option<u128> {
    let mut remaining = inclusive_tokens;
    let mut cost = 0_u128;
    for (reported, rate) in categories {
        if rate == base_rate {
            continue;
        }
        let tokens = reported?;
        remaining = remaining.checked_sub(tokens)?;
        cost = cost.checked_add(category_cost(tokens, rate)?)?;
    }
    cost.checked_add(category_cost(remaining, base_rate)?)
}

fn category_cost(tokens: u64, rate: Option<PicoUsdPerMillion>) -> Option<u128> {
    if tokens == 0 {
        return Some(0);
    }
    let rate = rate?;
    rate.value().checked_mul(u128::from(tokens))
}

fn catalog_rates(rates: CatalogModelCostRates) -> ModelPricing {
    ModelPricing {
        input_per_million_usd: Some(rates.input),
        output_per_million_usd: Some(rates.output),
        reasoning_per_million_usd: rates.reasoning,
        cache_read_per_million_usd: rates.cache_read,
        cache_write_per_million_usd: rates.cache_write,
    }
}

fn cost_numerator_to_usd(value: u128) -> f64 {
    let pico_usd = value / TOKENS_PER_MILLION;
    let fractional_pico_usd = value % TOKENS_PER_MILLION;
    ((pico_usd as f64) + (fractional_pico_usd as f64) / (TOKENS_PER_MILLION as f64))
        / (PICO_USD_PER_USD as f64)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cookie_agent_config::{ModelPricing, PicoUsdPerMillion, PricingConfig};
    use cookie_agent_models::catalog::{
        CatalogModelCost, CatalogModelCostRates, CatalogModelCostTier,
    };
    use cookie_agent_protocol::{ModelKey, ResolvedModelRef, Usage, UsageRollup};

    fn model(model: &str) -> ResolvedModelRef {
        crate::model_history::wire_model(&crate::test_support::model_binding_named(model))
    }

    fn reported(input: Option<u64>, output: Option<u64>, cache_read: Option<u64>) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            input_tokens_cache_read: cache_read,
            ..Usage::default()
        }
    }

    fn rate(value: &str) -> PicoUsdPerMillion {
        PicoUsdPerMillion::from_decimal_str(value).unwrap()
    }

    fn flat_rates(input: &str, output: &str) -> ModelPricing {
        ModelPricing {
            input_per_million_usd: Some(rate(input)),
            output_per_million_usd: Some(rate(output)),
            ..ModelPricing::default()
        }
    }

    #[test]
    fn records_multiple_turns_and_models_without_mixing_inclusive_totals() {
        let mut rollup = UsageRollup::default();
        super::record_stamped(
            &mut rollup,
            &model("fallback-zero"),
            &reported(Some(100), Some(20), Some(40)),
            None,
        );
        super::record_stamped(
            &mut rollup,
            &model("fallback-zero"),
            &reported(Some(50), Some(10), None),
            None,
        );
        super::record_stamped(
            &mut rollup,
            &model("fallback-one"),
            &Usage {
                input_tokens: Some(200),
                output_tokens: Some(30),
                input_tokens_cache_write: Some(25),
                ..Usage::default()
            },
            None,
        );
        assert_eq!(rollup.input_tokens, 350);
        assert_eq!(rollup.output_tokens, 60);
        assert_eq!(rollup.cache_read_tokens, 40);
        assert_eq!(rollup.cache_write_tokens, 25);
        assert_eq!(rollup.request_count, 3);
        assert_eq!(rollup.by_model.len(), 2);
    }

    #[test]
    fn missing_cache_observation_differs_from_observed_zero() {
        let key = "custom.test/fallback-zero".parse::<ModelKey>().unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([(
                key,
                ModelPricing {
                    cache_read_per_million_usd: Some(rate("0.5")),
                    ..flat_rates("1", "2")
                },
            )]),
        };
        let mut missing = UsageRollup::default();
        let missing_usage = reported(Some(100), Some(10), None);
        let missing_cost = super::estimated_cost_pico_usd(
            &model("fallback-zero"),
            &missing_usage,
            &pricing,
            &BTreeMap::new(),
        );
        super::record_stamped(
            &mut missing,
            &model("fallback-zero"),
            &missing_usage,
            missing_cost,
        );
        let missing = super::with_pricing(missing, &pricing, &BTreeMap::new());
        assert_eq!(missing.cache_hit_rate, None);
        assert_eq!(missing.estimated_cost_usd, None);

        let mut observed_zero = UsageRollup::default();
        let observed_usage = reported(Some(100), Some(10), Some(0));
        let observed_cost = super::estimated_cost_pico_usd(
            &model("fallback-zero"),
            &observed_usage,
            &pricing,
            &BTreeMap::new(),
        );
        super::record_stamped(
            &mut observed_zero,
            &model("fallback-zero"),
            &observed_usage,
            observed_cost,
        );
        let observed_zero = super::with_pricing(observed_zero, &pricing, &BTreeMap::new());
        assert_eq!(observed_zero.cache_hit_rate, Some(0.0));
        assert_eq!(observed_zero.estimated_cost_usd, Some(0.00012));
    }

    #[test]
    fn catalog_threshold_is_selected_at_not_before_boundary() {
        let key = "custom.test/fallback-zero".parse::<ModelKey>().unwrap();
        let catalog = BTreeMap::from([(
            key,
            CatalogModelCost {
                input: rate("1"),
                output: rate("2"),
                reasoning: None,
                cache_read: None,
                cache_write: None,
                context_over_200k: None,
                tiers: vec![CatalogModelCostTier {
                    context_tokens: 200_000,
                    rates: CatalogModelCostRates {
                        input: rate("3"),
                        output: rate("4"),
                        reasoning: None,
                        cache_read: None,
                        cache_write: None,
                    },
                }],
            },
        )]);
        let stamped = super::estimated_cost_pico_usd(
            &model("fallback-zero"),
            &reported(Some(200_000), Some(1), Some(0)),
            &PricingConfig::default(),
            &catalog,
        );
        assert_eq!(stamped, Some(600_004_000_000));
        assert_eq!(
            super::estimated_cost_pico_usd(
                &model("fallback-zero"),
                &reported(Some(200_000), Some(1), Some(0)),
                &PricingConfig::default(),
                &BTreeMap::new(),
            ),
            None
        );
    }

    #[test]
    fn stamped_costs_override_current_pricing() {
        let mut fully_stamped = UsageRollup::default();
        for cost in [500_000_000_000, 250_000_000_000] {
            super::record_stamped(
                &mut fully_stamped,
                &model("fallback-zero"),
                &reported(Some(100), Some(10), Some(0)),
                Some(cost),
            );
        }
        assert_eq!(
            super::with_pricing(fully_stamped, &PricingConfig::default(), &BTreeMap::new(),)
                .estimated_cost_usd,
            Some(0.75)
        );

        let key = "custom.test/fallback-zero".parse::<ModelKey>().unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([(key, flat_rates("1", "2"))]),
        };
        let mut stamped_unpriced = UsageRollup::default();
        super::record_stamped(
            &mut stamped_unpriced,
            &model("fallback-zero"),
            &reported(Some(1_000_000), Some(0), Some(0)),
            None,
        );
        assert_eq!(
            super::with_pricing(stamped_unpriced, &pricing, &BTreeMap::new()).estimated_cost_usd,
            None
        );
    }

    #[test]
    fn fixed_point_accumulation_is_stable_for_repeated_fractional_rates() {
        let key = "custom.test/fallback-zero".parse::<ModelKey>().unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([(key, flat_rates("0.333333333333", "0"))]),
        };
        let mut rollup = UsageRollup::default();
        for _ in 0..3 {
            let usage = reported(Some(1), Some(0), Some(0));
            let cost = super::estimated_cost_pico_usd(
                &model("fallback-zero"),
                &usage,
                &pricing,
                &BTreeMap::new(),
            );
            super::record_stamped(&mut rollup, &model("fallback-zero"), &usage, cost);
        }
        let priced = super::with_pricing(rollup, &pricing, &BTreeMap::new());
        assert_eq!(priced.estimated_cost_usd, Some(0.000000999999));
    }

    #[test]
    fn small_rate_across_large_volume_remains_nonzero() {
        let key = "custom.test/fallback-zero".parse::<ModelKey>().unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([(key, flat_rates("0.000000000001", "0"))]),
        };
        let mut rollup = UsageRollup::default();
        let usage = reported(Some(1_000_000), Some(0), Some(0));
        let cost = super::estimated_cost_pico_usd(
            &model("fallback-zero"),
            &usage,
            &pricing,
            &BTreeMap::new(),
        );
        super::record_stamped(&mut rollup, &model("fallback-zero"), &usage, cost);
        let priced = super::with_pricing(rollup, &pricing, &BTreeMap::new());
        assert_eq!(priced.estimated_cost_usd, Some(0.000000000001));
    }

    #[test]
    fn token_and_currency_overflow_return_no_cost() {
        let key = "custom.test/fallback-zero".parse::<ModelKey>().unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([(
                key,
                ModelPricing {
                    input_per_million_usd: Some(PicoUsdPerMillion::new(u128::MAX)),
                    output_per_million_usd: Some(rate("1")),
                    ..ModelPricing::default()
                },
            )]),
        };
        let mut currency = UsageRollup::default();
        super::record_stamped(
            &mut currency,
            &model("fallback-zero"),
            &reported(Some(2), Some(0), Some(0)),
            None,
        );
        assert_eq!(
            super::with_pricing(currency, &pricing, &BTreeMap::new()).estimated_cost_usd,
            None
        );

        let mut tokens = UsageRollup::default();
        super::record_stamped(
            &mut tokens,
            &model("fallback-zero"),
            &reported(Some(u64::MAX), Some(0), Some(0)),
            None,
        );
        super::record_stamped(
            &mut tokens,
            &model("fallback-zero"),
            &reported(Some(1), Some(0), Some(0)),
            None,
        );
        assert!(tokens.arithmetic_overflow);
        assert_eq!(
            super::with_pricing(tokens, &PricingConfig::default(), &BTreeMap::new())
                .estimated_cost_usd,
            None
        );
    }
}
