#![cfg(unix)]

use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::PermissionsExt as _,
    sync::{Arc, Mutex},
};

use cookie_agent_identity::{ProviderId, ProviderModelId};
use cookie_agent_models::catalog::{
    CATALOG_BODY_FILE, CATALOG_LOCK_FILE, CATALOG_MAX_BYTES, CATALOG_META_FILE, CatalogAgeState,
    CatalogAvailability, CatalogRequest, CatalogSource, CatalogTransport, CatalogTransportError,
    CatalogTransportFuture, CatalogTransportResponse, MODELS_DEV_CATALOG_URL,
};
use cookie_agent_models::{catalog::CatalogManager, secure_store::SecureDirectory};
use futures_util::stream;
use jiff::Timestamp;
use sha2::{Digest as _, Sha256};

#[derive(Clone, Default)]
struct ScriptedTransport {
    requests: Arc<Mutex<Vec<CatalogRequest>>>,
    responses: Arc<Mutex<VecDeque<Result<CatalogTransportResponse, CatalogTransportError>>>>,
}

impl ScriptedTransport {
    fn with(responses: impl IntoIterator<Item = CatalogTransportResponse>) -> Self {
        Self {
            requests: Arc::default(),
            responses: Arc::new(Mutex::new(responses.into_iter().map(Ok).collect())),
        }
    }

    fn failing() -> Self {
        Self {
            requests: Arc::default(),
            responses: Arc::new(Mutex::new(VecDeque::from([Err(
                CatalogTransportError::RequestFailed,
            )]))),
        }
    }
}

impl CatalogTransport for ScriptedTransport {
    fn fetch(&self, request: CatalogRequest) -> CatalogTransportFuture<'_> {
        self.requests.lock().unwrap().push(request);
        let response = self.responses.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { response })
    }
}

fn now() -> Timestamp {
    "2026-08-05T00:00:00Z".parse().unwrap()
}

fn candidate() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "providers": {
            "test": {
                "id": "test",
                "env": ["TEST_API_KEY"],
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://example.invalid/v1",
                "name": "Test",
                "doc": "https://example.invalid/docs",
                "models": {
                    "group/model": {
                        "id": "group/model",
                        "name": "Group Model",
                        "description": "test model",
                        "attachment": false,
                        "reasoning": true,
                        "tool_call": true,
                        "temperature": true,
                        "structured_output": true,
                        "open_weights": false,
                        "release_date": "2026-08-01",
                        "last_updated": "2026-08-02",
                        "modalities": {"input": ["text"], "output": ["text"]},
                        "limit": {"context": 8192, "output": 1024},
                        "reasoning_options": [{"type": "effort", "values": ["low", "high"]}]
                    }
                }
            }
        },
        "models": {
            "group/model": {
                "id": "group/model",
                "name": "Canonical",
                "description": "metadata only",
                "attachment": false,
                "reasoning": false,
                "tool_call": false,
                "temperature": false,
                "open_weights": false,
                "release_date": "2026-08-01",
                "last_updated": "2026-08-02",
                "modalities": {"input": ["audio"], "output": ["audio"]},
                "limit": {"context": 1, "output": 1}
            }
        }
    }))
    .unwrap()
}

#[tokio::test]
async fn catalog_model_costs_are_preserved_with_cache_and_reasoning_rates() {
    let temporary = tempfile::tempdir().unwrap();
    let mut document: serde_json::Value = serde_json::from_slice(&candidate()).unwrap();
    document["providers"]["test"]["models"]["group/model"]["cost"] = serde_json::json!({
        "input": "__EXACT_NUMERIC_RATE__",
        "output": 12.0,
        "reasoning": 15.0,
        "cache_read": 0.2,
        "cache_write": 2.5,
        "context_over_200k": {
            "input": 3.0,
            "output": 18.0,
            "reasoning": 20.0,
            "cache_read": 0.3,
            "cache_write": 3.5
        },
        "tiers": [{
            "input": 4.0,
            "output": 24.0,
            "reasoning": 25.0,
            "cache_read": 0.4,
            "cache_write": 4.5,
            "tier": {"type": "context", "size": 300000}
        }]
    });
    let document = serde_json::to_string(&document)
        .unwrap()
        .replace("\"__EXACT_NUMERIC_RATE__\"", "10000.000000000001");
    let snapshot = manager(
        ScriptedTransport::with([CatalogTransportResponse::from_bytes(
            200,
            document.into_bytes(),
        )]),
        &temporary,
    )
    .refresh_at(now())
    .await
    .unwrap();
    let cost = snapshot
        .model(
            &ProviderId::new("test").unwrap(),
            &ProviderModelId::new("group/model").unwrap(),
        )
        .unwrap()
        .record
        .as_ref()
        .unwrap()
        .cost
        .as_ref()
        .unwrap();
    assert_eq!(cost.input.value(), 10_000_000_000_000_001);
    assert_eq!(cost.output.value(), 12_000_000_000_000);
    assert_eq!(cost.reasoning.unwrap().value(), 15_000_000_000_000);
    assert_eq!(cost.cache_read.unwrap().value(), 200_000_000_000);
    assert_eq!(cost.cache_write.unwrap().value(), 2_500_000_000_000);
    assert_eq!(
        cost.context_over_200k.unwrap().input.value(),
        3_000_000_000_000
    );
    assert_eq!(cost.tiers[0].context_tokens, 300_000);
    assert_eq!(cost.tiers[0].rates.output.value(), 24_000_000_000_000);
    assert_eq!(
        cost.rates_for_input(199_999).input.value(),
        10_000_000_000_000_001
    );
    assert_eq!(
        cost.rates_for_input(200_000).input.value(),
        3_000_000_000_000
    );
    assert_eq!(
        cost.rates_for_input(300_000).input.value(),
        4_000_000_000_000
    );
}

fn manager(
    transport: ScriptedTransport,
    temporary: &tempfile::TempDir,
) -> CatalogManager<ScriptedTransport> {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory = SecureDirectory::open_in(temporary.path(), "catalog").unwrap();
    CatalogManager::new(transport, directory)
}

#[tokio::test]
async fn network_cache_etag_and_304_use_only_the_fixed_request() {
    let temporary = tempfile::tempdir().unwrap();
    let mut response = CatalogTransportResponse::from_bytes(200, candidate());
    response.etag = Some("\"revision-one\"".to_owned());
    let transport = ScriptedTransport::with([response, CatalogTransportResponse::not_modified()]);
    let requests = Arc::clone(&transport.requests);
    let manager = manager(transport, &temporary);

    let first = manager.refresh_at(now()).await.unwrap();
    assert_eq!(first.source, CatalogSource::Network);
    assert_eq!(first.state.availability, CatalogAvailability::Ready);
    assert_eq!(first.providers.len(), 1);
    let provider = ProviderId::new("test").unwrap();
    let model = ProviderModelId::new("group/model").unwrap();
    let record = first
        .model(&provider, &model)
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert_eq!(record.id.as_str(), "group/model");
    assert_eq!(record.name, "Group Model");
    assert_eq!(
        record.canonical_provenance.as_ref().unwrap().id.as_str(),
        "group/model"
    );
    let cache_root = temporary.path().join("catalog");
    assert!(cache_root.join(CATALOG_BODY_FILE).is_file());
    assert!(cache_root.join(CATALOG_META_FILE).is_file());
    assert!(cache_root.join(CATALOG_LOCK_FILE).is_file());
    let entries = fs::read_dir(&cache_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        entries,
        [CATALOG_BODY_FILE, CATALOG_META_FILE, CATALOG_LOCK_FILE]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );

    let second = manager.refresh_at(now()).await.unwrap();
    assert_eq!(second.source, CatalogSource::Network);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url, MODELS_DEV_CATALOG_URL);
    assert_eq!(requests[0].accept_encoding, "identity");
    assert_eq!(requests[0].if_none_match, None);
    assert_eq!(
        requests[1].if_none_match.as_deref(),
        Some("\"revision-one\"")
    );
}

#[tokio::test]
async fn malformed_provider_and_model_records_quarantine_locally() {
    let temporary = tempfile::tempdir().unwrap();
    let bytes = br#"{
      "providers": {
        "broken": {"id":"broken","env":["KEY"],"npm":"x","name":"Broken","doc":"https://example.invalid","models":{},"unknown":true},
        "test": {"id":"test","env":["KEY"],"npm":"x","name":"Test","doc":"https://example.invalid","models":{
          "bad":{"id":"bad","name":"Bad","description":"bad","attachment":false,"reasoning":false,"tool_call":false,"open_weights":false,"release_date":"2026-01-01","last_updated":"2026-01-01","modalities":{"input":["text"],"output":["text"]},"limit":{"context":1,"output":1},"unknown":true},
          "ok/model":{"id":"ok/model","name":"OK","description":"ok","attachment":false,"reasoning":false,"tool_call":false,"open_weights":false,"release_date":"2026-01-01","last_updated":"2026-01-01","modalities":{"input":["text"],"output":["text"]},"limit":{"context":1,"output":1}}
        }}
      },
      "models": {"ok/model":{"id":"ok/model","name":"OK","description":"ok","attachment":false,"reasoning":false,"tool_call":false,"open_weights":false,"release_date":"2026-01-01","last_updated":"2026-01-01","modalities":{"input":["text"],"output":["text"]},"limit":{"context":1,"output":1}}}
    }"#.to_vec();
    let snapshot = manager(
        ScriptedTransport::with([CatalogTransportResponse::from_bytes(200, bytes)]),
        &temporary,
    )
    .refresh_at(now())
    .await
    .unwrap();
    assert!(
        snapshot
            .provider(&ProviderId::new("broken").unwrap())
            .unwrap()
            .record
            .is_none()
    );
    let provider = snapshot
        .provider(&ProviderId::new("test").unwrap())
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert!(
        provider.models[&ProviderModelId::new("bad").unwrap()]
            .record
            .is_none()
    );
    assert!(
        provider.models[&ProviderModelId::new("ok/model").unwrap()]
            .record
            .is_some()
    );
}

#[tokio::test]
async fn recognized_ignored_fields_are_strictly_typed_and_quarantine_exact_records() {
    let temporary = tempfile::tempdir().unwrap();
    let mut document: serde_json::Value = serde_json::from_slice(&candidate()).unwrap();
    let provider = &mut document["providers"]["test"];
    let base_model = provider["models"]["group/model"].clone();
    let model_cases = [
        (
            "bad-cost",
            "cost",
            serde_json::json!({"input": "free", "output": 1}),
        ),
        ("bad-knowledge", "knowledge", serde_json::json!(true)),
        ("bad-interleaved", "interleaved", serde_json::json!(false)),
        (
            "bad-experimental",
            "experimental",
            serde_json::json!({"modes": []}),
        ),
        (
            "bad-provider-body",
            "provider",
            serde_json::json!({"body": []}),
        ),
    ];
    for (id, field, malformed) in model_cases {
        let mut model = base_model.clone();
        model["id"] = serde_json::json!(id);
        model[field] = malformed;
        provider["models"][id] = model;
    }

    let base_canonical = document["models"]["group/model"].clone();
    let canonical_cases = [
        ("bad-benchmarks", "benchmarks", serde_json::json!({})),
        ("bad-weights", "weights", serde_json::json!([{"url": 42}])),
        ("bad-license", "license", serde_json::json!(false)),
        (
            "bad-links",
            "links",
            serde_json::json!([{"url": "https://example.invalid", "unknown": true}]),
        ),
        (
            "bad-structured-output",
            "structured_output",
            serde_json::json!("yes"),
        ),
    ];
    for (id, field, malformed) in canonical_cases {
        let mut model = base_canonical.clone();
        model["id"] = serde_json::json!(id);
        model[field] = malformed;
        document["models"][id] = model;
    }

    let snapshot = manager(
        ScriptedTransport::with([CatalogTransportResponse::from_bytes(
            200,
            serde_json::to_vec(&document).unwrap(),
        )]),
        &temporary,
    )
    .refresh_at(now())
    .await
    .unwrap();
    let provider = snapshot
        .provider(&ProviderId::new("test").unwrap())
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert!(
        provider.models[&ProviderModelId::new("group/model").unwrap()]
            .record
            .is_some()
    );
    for id in [
        "bad-cost",
        "bad-knowledge",
        "bad-interleaved",
        "bad-experimental",
        "bad-provider-body",
    ] {
        assert!(
            provider.models[&ProviderModelId::new(id).unwrap()]
                .record
                .is_none(),
            "{id}"
        );
    }
    for id in [
        "bad-benchmarks",
        "bad-weights",
        "bad-license",
        "bad-links",
        "bad-structured-output",
    ] {
        assert!(
            snapshot
                .canonical_models
                .keys()
                .all(|model| model.as_str() != id),
            "{id}"
        );
    }
}

#[tokio::test]
async fn streamed_limit_and_compression_fail_without_exposing_a_body() {
    let temporary = tempfile::tempdir().unwrap();
    let oversized = CatalogTransportResponse {
        status: 200,
        content_type: Some("application/json".to_owned()),
        content_encoding: None,
        content_length: None,
        etag: None,
        body: Box::pin(stream::iter([
            Ok(vec![b' '; 8 * 1024 * 1024]),
            Ok(vec![b' '; 8 * 1024 * 1024 + 1]),
        ])),
    };
    let snapshot = manager(ScriptedTransport::with([oversized]), &temporary)
        .refresh_at(now())
        .await
        .unwrap();
    assert_eq!(snapshot.source, CatalogSource::Bootstrap);
    assert!(
        snapshot
            .state
            .last_error
            .as_ref()
            .unwrap()
            .safe_message
            .len()
            <= 512
    );

    let temporary = tempfile::tempdir().unwrap();
    let mut compressed = CatalogTransportResponse::from_bytes(200, candidate());
    compressed.content_encoding = Some("gzip".to_owned());
    let snapshot = manager(ScriptedTransport::with([compressed]), &temporary)
        .refresh_at(now())
        .await
        .unwrap();
    assert_eq!(snapshot.source, CatalogSource::Bootstrap);
}

#[tokio::test]
async fn validated_bootstrap_is_the_final_offline_source() {
    let temporary = tempfile::tempdir().unwrap();
    let snapshot = manager(ScriptedTransport::failing(), &temporary)
        .refresh_at(now())
        .await
        .unwrap();
    assert_eq!(snapshot.source, CatalogSource::Bootstrap);
    assert_eq!(snapshot.state.availability, CatalogAvailability::Bootstrap);
    assert!(!snapshot.providers.is_empty());
    let openai = snapshot
        .provider(&ProviderId::new("openai").unwrap())
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert_eq!(openai.npm, "@ai-sdk/openai");
    assert_eq!(openai.api, None);
    assert_eq!(openai.shape, None);
}

#[tokio::test]
async fn synthetic_metadata_fixture_retains_presence_and_quarantines_only_bad_sibling() {
    let temporary = tempfile::tempdir().unwrap();
    let bytes = include_bytes!("fixtures/models-dev-metadata-synthetic.json").to_vec();
    let metadata: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/models-dev-metadata-synthetic.meta.json"
    ))
    .unwrap();
    assert_eq!(metadata["runtime_pin"], false);
    assert_eq!(metadata["contains_secrets"], false);
    assert_eq!(metadata["approved_live_audit"], false);
    assert_eq!(metadata["byte_length"], bytes.len() as u64);
    assert_eq!(metadata["sha256"], format!("{:x}", Sha256::digest(&bytes)));

    let snapshot = manager(
        ScriptedTransport::with([CatalogTransportResponse::from_bytes(200, bytes)]),
        &temporary,
    )
    .refresh_at(now())
    .await
    .unwrap();
    let openai = snapshot
        .provider(&ProviderId::new("openai").unwrap())
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert_eq!(openai.environment, ["OPENAI_API_KEY"]);
    assert_eq!(openai.npm, "@ai-sdk/openai");
    assert_eq!(openai.api, None);
    assert_eq!(openai.shape, None);
    let overridden = openai.models[&ProviderModelId::new("compat/audit").unwrap()]
        .record
        .as_ref()
        .unwrap();
    assert_eq!(overridden.shape.as_deref(), Some("responses"));
    let override_metadata = overridden.provider.as_ref().unwrap();
    assert_eq!(
        override_metadata.npm.as_deref(),
        Some("@ai-sdk/openai-compatible")
    );
    assert_eq!(
        override_metadata.api.as_deref(),
        Some("https://example.invalid/v1")
    );
    assert_eq!(override_metadata.shape.as_deref(), Some("completions"));

    let shape_audit = snapshot
        .provider(&ProviderId::new("shape-audit").unwrap())
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert!(shape_audit.environment.is_empty());
    assert!(shape_audit.npm.is_empty());
    assert_eq!(shape_audit.api, None);
    assert_eq!(shape_audit.shape.as_deref(), Some("provider-shape-v1"));
    assert!(
        shape_audit.models[&ProviderModelId::new("sibling-good").unwrap()]
            .record
            .is_some()
    );
    assert!(
        shape_audit.models[&ProviderModelId::new("sibling-bad").unwrap()]
            .record
            .is_none()
    );
}

#[tokio::test]
async fn duplicate_root_keys_reject_only_the_network_candidate() {
    let temporary = tempfile::tempdir().unwrap();
    let duplicate_root = br#"{
      "providers": {},
      "providers": {"test":{"id":"test","env":["KEY"],"npm":"x","name":"Test","doc":"https://example.invalid","models":{}}},
      "models": {"x":{"id":"x","name":"X","description":"x","attachment":false,"reasoning":false,"tool_call":false,"open_weights":false,"release_date":"2026-01-01","last_updated":"2026-01-01","modalities":{"input":["text"],"output":["text"]},"limit":{"context":1,"output":1}}}
    }"#
    .to_vec();
    let snapshot = manager(
        ScriptedTransport::with([CatalogTransportResponse::from_bytes(200, duplicate_root)]),
        &temporary,
    )
    .refresh_at(now())
    .await
    .unwrap();
    assert_eq!(snapshot.source, CatalogSource::Bootstrap);
}

#[tokio::test]
async fn cache_has_no_hard_ttl_and_reports_thirty_day_warning() {
    let temporary = tempfile::tempdir().unwrap();
    let transport = ScriptedTransport {
        requests: Arc::default(),
        responses: Arc::new(Mutex::new(VecDeque::from([
            Ok(CatalogTransportResponse::from_bytes(200, candidate())),
            Err(CatalogTransportError::RequestFailed),
        ]))),
    };
    let manager = manager(transport, &temporary);
    manager
        .refresh_at("2026-07-01T00:00:00Z".parse().unwrap())
        .await
        .unwrap();
    let stale = manager.refresh_at(now()).await.unwrap();
    assert_eq!(stale.source, CatalogSource::Cache);
    assert_eq!(stale.state.availability, CatalogAvailability::Stale);
    assert_eq!(stale.state.age, CatalogAgeState::OlderThanThirtyDays);
}

#[tokio::test]
async fn content_length_is_rejected_before_body_streaming() {
    let temporary = tempfile::tempdir().unwrap();
    let response = CatalogTransportResponse {
        status: 200,
        content_type: Some("application/json".to_owned()),
        content_encoding: None,
        content_length: Some(CATALOG_MAX_BYTES as u64 + 1),
        etag: None,
        body: Box::pin(stream::once(async {
            panic!("oversized Content-Length must reject before body polling")
        })),
    };
    let snapshot = manager(ScriptedTransport::with([response]), &temporary)
        .refresh_at(now())
        .await
        .unwrap();
    assert_eq!(snapshot.source, CatalogSource::Bootstrap);
}
