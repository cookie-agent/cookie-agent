#![cfg(unix)]

//! Wire-level request bodies for models.dev reasoning variants, per adapter
//! family.

use std::{
    collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc, time::Duration,
};

use cookie_agent_identity::{
    CatalogRevision, ModelSelection, ProviderId, ProviderModelId, VariantId,
};
use cookie_agent_models::{
    ModelManager, ProviderDefinition,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry, CatalogProviderRecord,
        CatalogReasoningOption, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

struct Catalog {
    provider: &'static str,
    npm: &'static str,
    model: &'static str,
    output: u64,
    options: Vec<CatalogReasoningOption>,
}

fn effort(values: &[&str]) -> CatalogReasoningOption {
    CatalogReasoningOption::Effort {
        values: values
            .iter()
            .map(|value| Some((*value).to_owned()))
            .collect(),
    }
}

fn budget(min: Option<i64>, max: Option<i64>) -> CatalogReasoningOption {
    CatalogReasoningOption::BudgetTokens { min, max }
}

fn snapshot(catalog: &Catalog, api: String) -> Arc<CatalogSnapshot> {
    let provider_id = ProviderId::new(catalog.provider).unwrap();
    let model_id = ProviderModelId::new(catalog.model).unwrap();
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: catalog.model.to_owned(),
        description: "test".to_owned(),
        family: None,
        attachment: false,
        reasoning: true,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(false),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 200_000,
            input: None,
            output: catalog.output,
        },
        shape: None,
        provider: None,
        reasoning_options: catalog.options.clone(),
        cost: None,
        interleaved: None,
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: catalog.provider.to_owned(),
        environment: vec!["TEST_API_KEY".to_owned()],
        npm: catalog.npm.to_owned(),
        api: Some(api),
        shape: None,
        documentation_url: "https://example.test/docs".to_owned(),
        models: BTreeMap::from([(
            model_id.clone(),
            CatalogModelEntry {
                id: model_id,
                record: Some(model),
                quarantine: None,
            },
        )]),
    };
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!(
            "sha256:{:x}",
            Sha256::digest(b"reasoning-request-catalog")
        ))
        .unwrap(),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::from([(
            provider_id.clone(),
            CatalogProviderEntry {
                id: provider_id,
                record: Some(record),
                quarantine: None,
            },
        )]),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

fn manager(catalog: &Catalog, api: String, temporary: &TempDir) -> ModelManager {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = ProviderStore::open(temporary.path().join("providers")).unwrap();
    let definition =
        toml::from_str::<ProviderDefinition>("source = \"models_dev\"\napi_key = \"test-key\"\n")
            .unwrap();
    ModelManager::new(
        BTreeMap::from([(ProviderId::new(catalog.provider).unwrap(), definition)]),
        snapshot(catalog, api),
        store,
    )
    .unwrap()
}

fn variant_names(catalog: &Catalog) -> Vec<String> {
    let temporary = TempDir::new().unwrap();
    let manager = manager(catalog, "http://127.0.0.1:9/v1".to_owned(), &temporary);
    let runtime = manager.current();
    let model = &runtime.models().values().next().unwrap().model;
    model
        .variant_order
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

async fn capture_http_request() -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8192];
        let mut expected = None;
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if expected.is_none()
                && let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                expected = Some(header_end + content_length);
            }
            if expected.is_some_and(|expected| request.len() >= expected) {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{address}/v1"), task)
}

/// Sends one request through the named generated variant and returns the
/// captured JSON body.
async fn request_body(catalog: &Catalog, variant: &str) -> Value {
    request_body_with_output_cap(catalog, variant, None).await
}

/// Like [`request_body`], with the request's `max_output_tokens` set as the
/// engine sets it (the model's output limit, or a smaller agent cap).
async fn request_body_with_output_cap(
    catalog: &Catalog,
    variant: &str,
    max_output_tokens: Option<u64>,
) -> Value {
    let temporary = TempDir::new().unwrap();
    let (endpoint, captured) = capture_http_request().await;
    let manager = manager(catalog, endpoint, &temporary);
    let resolved = manager
        .current()
        .resolve(&ModelSelection {
            model: format!("{}/{}", catalog.provider, catalog.model)
                .parse()
                .unwrap(),
            variant: Some(VariantId::new(variant).unwrap()),
        })
        .unwrap();
    let mut request = oven_sdk::Request::new(vec![oven_sdk::HistoryTurn::user(
        oven_sdk::UserMessage::new(vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
            "hello",
        ))]),
    )]);
    request.inference.max_output_tokens = max_output_tokens;
    let error = resolved
        .model()
        .stream(
            resolved.prepare_request(request),
            oven_sdk::AbortSignal::default(),
        )
        .await
        .expect_err("mock server fails the request");
    let wire = tokio::time::timeout(Duration::from_secs(5), captured)
        .await
        .unwrap_or_else(|_| panic!("variant `{variant}` sent no request: {error:?}"))
        .unwrap();
    serde_json::from_str(wire.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn anthropic(model: &'static str, output: u64, options: Vec<CatalogReasoningOption>) -> Catalog {
    Catalog {
        provider: "anthropic",
        npm: "@ai-sdk/anthropic",
        model,
        output,
        options,
    }
}

fn compatible(
    provider: &'static str,
    model: &'static str,
    options: Vec<CatalogReasoningOption>,
) -> Catalog {
    Catalog {
        provider,
        npm: "@ai-sdk/openai-compatible",
        model,
        output: 131_072,
        options,
    }
}

const ADAPTIVE: fn() -> Value = || json!({ "type": "adaptive", "display": "summarized" });

#[tokio::test]
async fn anthropic_effort_variants_enable_summarized_adaptive_thinking() {
    let catalog = anthropic(
        "claude-opus-4-8",
        128_000,
        vec![effort(&["low", "medium", "high", "xhigh", "max"])],
    );
    assert_eq!(
        variant_names(&catalog),
        ["low", "medium", "high", "xhigh", "max"]
    );

    let body = request_body(&catalog, "xhigh").await;
    assert_eq!(body["thinking"], ADAPTIVE());
    assert_eq!(body["output_config"]["effort"], "xhigh");
    assert_eq!(body["max_tokens"], 128_000);
}

#[tokio::test]
async fn anthropic_toggle_off_disables_thinking_and_effort_keeps_it_on() {
    // Claude Sonnet 5 thinks by default, so `off` must be explicit.
    let catalog = anthropic(
        "claude-sonnet-5",
        128_000,
        vec![
            CatalogReasoningOption::Toggle,
            effort(&["low", "medium", "high", "xhigh", "max"]),
        ],
    );
    assert_eq!(
        variant_names(&catalog),
        ["off", "low", "medium", "high", "xhigh", "max"]
    );

    let off = request_body(&catalog, "off").await;
    assert_eq!(off["thinking"], json!({ "type": "disabled" }));
    assert!(off.get("output_config").is_none());

    let low = request_body(&catalog, "low").await;
    assert_eq!(low["thinking"], ADAPTIVE());
    assert_eq!(low["output_config"]["effort"], "low");
}

#[tokio::test]
async fn anthropic_toggle_only_on_requests_summarized_adaptive_thinking() {
    let catalog = anthropic(
        "claude-sonnet-5",
        128_000,
        vec![CatalogReasoningOption::Toggle],
    );
    assert_eq!(variant_names(&catalog), ["off", "on"]);

    let on = request_body(&catalog, "on").await;
    assert_eq!(on["thinking"], ADAPTIVE());
    assert!(on.get("output_config").is_none());
}

#[tokio::test]
async fn anthropic_budget_variants_send_manual_thinking_budgets() {
    // Claude Haiku 4.5: models.dev gives only `min: 1024`.
    let catalog = anthropic("claude-haiku-4-5", 64_000, vec![budget(Some(1024), None)]);
    assert_eq!(
        variant_names(&catalog),
        ["budget-min", "budget-high", "budget-max"]
    );

    for (variant, budget_tokens) in [
        ("budget-min", 1024),
        ("budget-high", 16_000),
        ("budget-max", 31_999),
    ] {
        let body = request_body(&catalog, variant).await;
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "budget_tokens": budget_tokens, "display": "summarized" }),
            "{variant}"
        );
        assert_eq!(body["max_tokens"], 64_000, "{variant}");
        assert!(body.get("output_config").is_none(), "{variant}");
    }
}

#[tokio::test]
async fn anthropic_budget_variants_fit_the_output_limit_with_full_or_capped_output() {
    // Claude Opus 4.1: 32K output. The engine sends the full output limit (or a
    // smaller agent cap) as visible output, and the Anthropic adapter adds the
    // thinking budget on top, so visible output must shrink to fit.
    let catalog = anthropic("claude-opus-4-1", 32_000, vec![budget(Some(1024), None)]);
    assert_eq!(
        variant_names(&catalog),
        ["budget-min", "budget-high", "budget-max"]
    );
    for (cap, budget_tokens, max_tokens) in [
        // Full output limit: visible output is what the budget leaves.
        (32_000, 27_904, 32_000),
        (32_000, 1024, 32_000),
        // A smaller agent cap that still fits is kept.
        (2_048, 27_904, 2_048 + 27_904),
        // A cap that does not fit beside the budget is lowered.
        (8_000, 27_904, 32_000),
    ] {
        let variant = if budget_tokens == 1024 {
            "budget-min"
        } else {
            "budget-max"
        };
        let body = request_body_with_output_cap(&catalog, variant, Some(cap)).await;
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "budget_tokens": budget_tokens, "display": "summarized" }),
            "{variant} {cap}"
        );
        assert_eq!(body["max_tokens"], max_tokens, "{variant} {cap}");
    }
}

#[tokio::test]
async fn anthropic_effort_on_budget_only_models_uses_a_manual_budget() {
    // Claude Opus 4.5 rejects adaptive thinking but accepts effort.
    let catalog = anthropic(
        "claude-opus-4-5",
        64_000,
        vec![effort(&["low", "medium", "high"]), budget(Some(1024), None)],
    );
    assert_eq!(
        variant_names(&catalog),
        [
            "low",
            "medium",
            "high",
            "budget-min",
            "budget-high",
            "budget-max"
        ]
    );

    let body = request_body(&catalog, "high").await;
    assert_eq!(
        body["thinking"],
        json!({ "type": "enabled", "budget_tokens": 16_000, "display": "summarized" })
    );
    assert_eq!(body["output_config"]["effort"], "high");
}

#[tokio::test]
async fn deepseek_off_disables_thinking_and_effort_sends_reasoning_effort() {
    let catalog = compatible(
        "deepseek",
        "deepseek-v4-pro",
        vec![
            CatalogReasoningOption::Toggle,
            effort(&["low", "high", "max"]),
        ],
    );
    assert_eq!(variant_names(&catalog), ["off", "low", "high", "max"]);

    let off = request_body(&catalog, "off").await;
    assert_eq!(off["thinking"], json!({ "type": "disabled" }));
    assert!(off.get("reasoning_effort").is_none());

    let max = request_body(&catalog, "max").await;
    assert_eq!(max["reasoning_effort"], "max");
    assert!(max.get("thinking").is_none());
}

#[tokio::test]
async fn kimi_code_off_sends_reasoning_effort_none() {
    let catalog = compatible(
        "kimi-code-plan-cn",
        "k3",
        vec![
            CatalogReasoningOption::Toggle,
            effort(&["low", "high", "max"]),
        ],
    );
    assert_eq!(variant_names(&catalog), ["off", "low", "high", "max"]);

    let off = request_body(&catalog, "off").await;
    assert_eq!(off["reasoning_effort"], "none");
    assert!(off.get("thinking").is_none());

    let high = request_body(&catalog, "high").await;
    assert_eq!(high["reasoning_effort"], "high");
}

#[tokio::test]
async fn glm_toggle_variants_send_thinking_type() {
    let catalog = compatible("zai", "glm-4.7", vec![CatalogReasoningOption::Toggle]);
    assert_eq!(variant_names(&catalog), ["off", "on"]);

    for (variant, kind) in [("off", "disabled"), ("on", "enabled")] {
        let body = request_body(&catalog, variant).await;
        assert_eq!(body["thinking"], json!({ "type": kind }), "{variant}");
        assert!(body.get("reasoning_effort").is_none(), "{variant}");
    }
}

#[tokio::test]
async fn model_studio_toggle_variants_send_enable_thinking() {
    // Budgets are not sent on this wire, so they neither appear nor suppress
    // the toggle's `on`.
    let catalog = compatible(
        "alibaba",
        "qwen3.5-plus",
        vec![CatalogReasoningOption::Toggle, budget(None, None)],
    );
    assert_eq!(variant_names(&catalog), ["off", "on"]);

    for (variant, enabled) in [("off", false), ("on", true)] {
        let body = request_body(&catalog, variant).await;
        assert_eq!(body["enable_thinking"], enabled, "{variant}");
    }
}

#[test]
fn compatible_toggles_without_a_documented_switch_are_omitted() {
    // Moonshot's kimi-k3 cannot turn thinking off and no switch is documented
    // for the provider as a whole.
    let catalog = compatible(
        "moonshotai",
        "kimi-k3",
        vec![
            CatalogReasoningOption::Toggle,
            effort(&["low", "high", "max"]),
        ],
    );
    assert_eq!(variant_names(&catalog), ["low", "high", "max"]);
}

#[tokio::test]
async fn anthropic_compatible_variants_use_the_anthropic_thinking_shape() {
    let catalog = Catalog {
        provider: "kimi-for-coding",
        npm: "@ai-sdk/anthropic",
        model: "k3",
        output: 32_768,
        options: vec![
            CatalogReasoningOption::Toggle,
            effort(&["low", "high", "max"]),
        ],
    };
    assert_eq!(variant_names(&catalog), ["off", "low", "high", "max"]);

    let off = request_body(&catalog, "off").await;
    assert_eq!(off["thinking"], json!({ "type": "disabled" }));

    let high = request_body(&catalog, "high").await;
    assert_eq!(high["thinking"], ADAPTIVE());
    assert_eq!(high["output_config"]["effort"], "high");
}
