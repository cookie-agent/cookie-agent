use cookie_agent_identity::{ProviderId, ProviderModelId};
use serde_json::{Value, json};

use super::{ParsedCatalog, parse_catalog};
use crate::catalog::{CatalogModelRecord, CatalogQuarantineReason};

fn model(id: &str) -> Value {
    json!({
        "id": id,
        "name": "Model",
        "description": "test model",
        "attachment": false,
        "reasoning": false,
        "tool_call": true,
        "open_weights": false,
        "release_date": "2026-09-01",
        "last_updated": "2026-09-28",
        "modalities": {"input": ["text"], "output": ["text"]},
        "limit": {"context": 8192, "output": 1024}
    })
}

fn canonical(id: &str) -> Value {
    json!({
        "id": id,
        "name": "Canonical",
        "description": "metadata only",
        "attachment": false,
        "reasoning": false,
        "tool_call": true,
        "open_weights": false,
        "release_date": "2026-09-01",
        "last_updated": "2026-09-28",
        "modalities": {"input": ["text"], "output": ["text"]},
        "limit": {"context": 8192}
    })
}

fn catalog(models: impl IntoIterator<Item = Value>) -> Value {
    let models = models
        .into_iter()
        .map(|model| (model["id"].as_str().unwrap().to_owned(), model))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "providers": {
            "test": {
                "id": "test",
                "env": ["TEST_API_KEY"],
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://example.invalid/v1",
                "name": "Test",
                "doc": "https://example.invalid/docs",
                "models": models
            }
        },
        "models": {"vendor/model": canonical("vendor/model")}
    })
}

fn parse(document: &Value) -> ParsedCatalog {
    parse_catalog(&serde_json::to_vec(document).unwrap()).unwrap()
}

fn record<'a>(parsed: &'a ParsedCatalog, provider: &str, model: &str) -> &'a CatalogModelRecord {
    parsed.providers[&ProviderId::new(provider).unwrap()]
        .record
        .as_ref()
        .unwrap_or_else(|| panic!("provider {provider} is quarantined"))
        .models[&ProviderModelId::new(model).unwrap()]
        .record
        .as_ref()
        .unwrap_or_else(|| panic!("model {provider}/{model} is quarantined"))
}

fn quarantined(parsed: &ParsedCatalog, model: &str) -> bool {
    parsed.providers[&ProviderId::new("test").unwrap()]
        .record
        .as_ref()
        .unwrap()
        .models[&ProviderModelId::new(model).unwrap()]
        .record
        .is_none()
}

#[test]
fn unknown_fields_are_ignored_at_every_level() {
    let mut forward = model("forward");
    forward["canonical_model_id"] = json!("vendor/model");
    forward["future_field"] = json!({"nested": [1, 2, 3]});
    forward["modalities"]["future"] = json!(true);
    forward["limit"]["future"] = json!(1);
    forward["reasoning"] = json!(true);
    forward["reasoning_options"] = json!([
        {"type": "effort", "values": ["low", "high"], "future": 1},
        {"type": "toggle", "future": 1},
        {"type": "budget_tokens", "min": 1024, "future": 1}
    ]);
    forward["interleaved"] = json!({"field": "reasoning_content", "future": 1});
    forward["provider"] = json!({"npm": "@ai-sdk/openai", "future": 1});
    forward["cost"] = json!({
        "input": 1,
        "output": 2,
        "future": 3,
        "context_over_200k": {"input": 2, "output": 4, "future": 5},
        "tiers": [{
            "input": 3,
            "output": 6,
            "future": 7,
            "tier": {"type": "context", "size": 300000, "future": 8}
        }]
    });
    // Metadata cookie never reads is ignored whatever its shape.
    forward["knowledge"] = json!(true);
    forward["experimental"] = json!({"modes": []});
    let mut document = catalog([forward]);
    document["future_root_key"] = json!({"anything": true});
    document["providers"]["test"]["future_provider_key"] = json!([1]);
    document["models"]["vendor/model"]["future_canonical_key"] = json!(1);
    document["models"]["vendor/model"]["benchmarks"] = json!({});
    document["models"]["vendor/model"]["links"] = json!([{"unknown": true}]);

    let parsed = parse(&document);

    assert_eq!(parsed.quarantine, []);
    assert_eq!(parsed.canonical_models.len(), 1);
    let record = record(&parsed, "test", "forward");
    assert_eq!(record.reasoning_options.len(), 3);
    assert_eq!(
        record.provider.as_ref().unwrap().npm.as_deref(),
        Some("@ai-sdk/openai")
    );
    let cost = record.cost.as_ref().unwrap();
    assert_eq!(cost.input.value(), 1_000_000_000_000);
    assert_eq!(cost.tiers[0].context_tokens, 300_000);
}

#[test]
fn inexact_catalog_prices_round_to_the_nearest_pico_usd() {
    let mut priced = model("priced");
    priced["cost"] = json!({
        "input": 0.010399999999999998,
        "output": "0.18749999999999994",
        "cache_read": 0.0000000000004
    });
    let parsed = parse(&catalog([priced]));

    assert_eq!(parsed.quarantine, []);
    let cost = record(&parsed, "test", "priced").cost.clone().unwrap();
    assert_eq!(cost.input.value(), 10_400_000_000);
    assert_eq!(cost.output.value(), 187_500_000_000);
    assert_eq!(cost.cache_read.unwrap().value(), 0);
}

#[test]
fn padded_display_text_is_trimmed() {
    let mut padded = model("padded");
    padded["name"] = json!("Padded Model \t");
    padded["description"] = json!("  described ");
    let mut document = catalog([padded]);
    document["providers"]["test"]["name"] = json!(" Test ");

    let parsed = parse(&document);

    assert_eq!(parsed.quarantine, []);
    let record = record(&parsed, "test", "padded");
    assert_eq!(record.name, "Padded Model");
    assert_eq!(record.description, "described");
    let provider = parsed.providers[&ProviderId::new("test").unwrap()]
        .record
        .as_ref()
        .unwrap();
    assert_eq!(provider.name, "Test");
}

#[test]
fn empty_modality_arrays_are_accepted() {
    let mut empty = model("empty");
    empty["modalities"] = json!({"input": [], "output": []});
    let parsed = parse(&catalog([empty]));

    assert_eq!(parsed.quarantine, []);
    let modalities = &record(&parsed, "test", "empty").modalities;
    assert!(modalities.input.is_empty() && modalities.output.is_empty());
}

#[test]
fn genuinely_invalid_records_still_quarantine() {
    let mut cases = Vec::new();
    let mut missing = model("missing-required");
    missing.as_object_mut().unwrap().remove("limit");
    cases.push(missing);
    let mut wrong_type = model("wrong-type");
    wrong_type["attachment"] = json!("no");
    cases.push(wrong_type);
    let mut blank_name = model("blank-name");
    blank_name["name"] = json!(" \t ");
    cases.push(blank_name);
    let mut control = model("control-name");
    control["name"] = json!("bad\u{7}name");
    cases.push(control);
    let mut bad_cost = model("bad-cost");
    bad_cost["cost"] = json!({"input": "free", "output": 1});
    cases.push(bad_cost);
    let mut missing_cost = model("missing-cost-output");
    missing_cost["cost"] = json!({"input": 1});
    cases.push(missing_cost);
    let mut bad_status = model("bad-status");
    bad_status["status"] = json!("retired");
    cases.push(bad_status);
    let mut bad_interleaved = model("bad-interleaved");
    bad_interleaved["interleaved"] = json!(false);
    cases.push(bad_interleaved);
    let mut mismatch = model("mismatch");
    mismatch["id"] = json!("other");
    let mut document = catalog(cases);
    document["providers"]["test"]["models"]["mismatch"] = mismatch;
    document["providers"]["test"]["models"]["ok"] = model("ok");

    let parsed = parse(&document);

    for id in [
        "missing-required",
        "wrong-type",
        "blank-name",
        "control-name",
        "bad-cost",
        "missing-cost-output",
        "bad-status",
        "bad-interleaved",
        "mismatch",
    ] {
        assert!(quarantined(&parsed, id), "{id}");
    }
    assert!(!quarantined(&parsed, "ok"));
    assert!(parsed.quarantine.iter().any(|entry| {
        entry.model_id.as_deref() == Some("mismatch")
            && entry.reason == CatalogQuarantineReason::ProviderModelIdentityMismatch
    }));

    let mut no_providers = catalog([model("ok")]);
    no_providers.as_object_mut().unwrap().remove("providers");
    assert!(parse_catalog(&serde_json::to_vec(&no_providers).unwrap()).is_err());
}

/// Every provider model in real models.dev captures must parse. The
/// 2026-09-28 sample is cut from the live catalog the day upstream added
/// `canonical_model_id`, with records that carry inexact float prices and
/// padded names.
#[test]
fn real_catalog_captures_parse_without_quarantine() {
    let sample = include_bytes!("../../tests/fixtures/models-dev-live-sample-2026-09-28.json");
    let parsed = parse_catalog(sample).unwrap();
    assert_eq!(parsed.quarantine, []);
    assert_eq!(parsed.providers.len(), 15);
    record(&parsed, "anthropic", "claude-opus-5-5");
    record(&parsed, "openai", "gpt-5");
    let kimi = parsed.providers[&ProviderId::new("kimi-code-plan-cn").unwrap()]
        .record
        .as_ref()
        .unwrap();
    assert_eq!(kimi.models.len(), 4);
    assert!(kimi.models.values().all(|entry| entry.record.is_some()));
    let venice = record(&parsed, "venice", "mercury-2-5")
        .cost
        .clone()
        .unwrap();
    assert_eq!(venice.input.value(), 50_000_000_000);
    assert_eq!(venice.output.value(), 187_500_000_000);
    assert_eq!(
        record(&parsed, "kilo", "bytedance/ui-tars-1.5-7b").name,
        "ByteDance: UI-TARS 7B"
    );

    for capture in [
        crate::catalog::MODELS_DEV_BOOTSTRAP,
        include_bytes!("../../tests/fixtures/models-dev-live-audit-2026-08-05.json"),
    ] {
        assert_eq!(parse_catalog(capture).unwrap().quarantine, []);
    }
}
