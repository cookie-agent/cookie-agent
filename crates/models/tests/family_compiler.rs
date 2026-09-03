use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, ProviderId, ProviderModelId, SetupFieldId,
};
use cookie_agent_models::{
    AuthOverride, BoundedSetupString, HeaderName, ManagedModelShape, ModelsDevProvider,
    ProviderDefinition, SafeSetupValue, SafeStaticHeaderValue, SecretString,
    adapters::OvenAdapterFamily,
    catalog::{
        CatalogInterleaved, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelProviderMetadata, CatalogModelRecord, CatalogModelStatus,
        CatalogProviderRecord, CatalogReasoningOption,
    },
    compiler::{CompiledModelStatus, DynamicCompileError, DynamicCompiler},
};

fn record(npm: &str, api: Option<&str>) -> CatalogProviderRecord {
    let model_id = ProviderModelId::new("model/1").unwrap();
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: "Model".into(),
        description: "Model".into(),
        family: None,
        attachment: true,
        reasoning: true,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".into(),
        last_updated: "2026-01-01".into(),
        modalities: CatalogModalities {
            input: vec!["text".into(), "image".into(), "pdf".into()],
            output: vec!["text".into()],
        },
        limits: CatalogLimits {
            context: 100_000,
            input: None,
            output: 10_000,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        cost: None,
        interleaved: Some(CatalogInterleaved::Reasoning),
        canonical_provenance: None,
    };
    CatalogProviderRecord {
        id: ProviderId::new("example").unwrap(),
        name: "Example".into(),
        environment: vec!["EXAMPLE_API_KEY".into()],
        npm: npm.into(),
        api: api.map(str::to_owned),
        shape: None,
        documentation_url: "https://example.com".into(),
        models: BTreeMap::from([(
            model_id.clone(),
            CatalogModelEntry {
                id: model_id,
                record: Some(model),
                quarantine: None,
            },
        )]),
    }
}

fn auth(method: &str, fields: &[(&str, &str)]) -> AuthOverride {
    AuthOverride {
        method: AuthMethodId::new(method).unwrap(),
        values: fields
            .iter()
            .map(|(name, value)| {
                (
                    AuthFieldName::new(*name).unwrap(),
                    SecretString::new(*value).unwrap(),
                )
            })
            .collect(),
    }
}

fn setup(fields: &[(&str, &str)]) -> BTreeMap<SetupFieldId, SafeSetupValue> {
    fields
        .iter()
        .map(|(name, value)| {
            (
                SetupFieldId::new(*name).unwrap(),
                SafeSetupValue::String(BoundedSetupString::new(*value).unwrap()),
            )
        })
        .collect()
}

#[test]
fn derives_capabilities_and_reasoning_field() {
    let compiled = DynamicCompiler::family_registry()
        .compile_managed(
            "sha256:test",
            &record("@ai-sdk/openai-compatible", Some("https://example.com/v1")),
            None,
        )
        .unwrap();
    let model = compiled.models.values().next().unwrap();
    assert!(model.capabilities.tool_calling);
    assert!(model.capabilities.structured_output);
    assert!(model.capabilities.reasoning);
    assert!(model.capabilities.temperature);
    assert_eq!(model.capabilities.context_tokens, 100_000);
    assert_eq!(model.capabilities.output_tokens, 10_000);
    assert_eq!(model.reasoning_field, "reasoning");
    assert!(
        model
            .capabilities
            .media
            .contains_key(&cookie_agent_models::MediaKind::Image)
    );
    assert!(
        model
            .capabilities
            .media
            .contains_key(&cookie_agent_models::MediaKind::Pdf)
    );
}

#[test]
fn projects_catalog_video_input_with_consensus_limits() {
    let mut provider = record("@ai-sdk/google", Some("https://example.com/v1"));
    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .modalities
        .input = vec!["text".into(), "image".into(), "video".into()];
    let compiled = DynamicCompiler::family_registry()
        .compile_managed("sha256:video", &provider, None)
        .unwrap();
    let capabilities = &compiled.models.values().next().unwrap().capabilities;
    assert!(
        capabilities
            .input
            .contains(&cookie_agent_models::Modality::Video)
    );
    let video = &capabilities.media[&cookie_agent_models::MediaKind::Video];
    assert_eq!(video.max_bytes, 25 * 1024 * 1024);
    assert_eq!(video.max_count, 2);
    assert_eq!(
        video
            .mime_types
            .iter()
            .map(|value| value.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "video/3gpp",
            "video/avi",
            "video/mov",
            "video/mp4",
            "video/mpeg",
            "video/mpg",
            "video/webm",
            "video/wmv",
            "video/x-flv",
        ])
    );

    for npm in ["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"] {
        let mut provider = record(npm, Some("https://example.com/v1"));
        provider
            .models
            .values_mut()
            .next()
            .unwrap()
            .record
            .as_mut()
            .unwrap()
            .modalities
            .input = vec!["text".into(), "video".into()];
        let compiled = DynamicCompiler::family_registry()
            .compile_managed("sha256:user-turn-video", &provider, None)
            .unwrap();
        let capabilities = &compiled.models.values().next().unwrap().capabilities;
        assert!(
            capabilities
                .input
                .contains(&cookie_agent_models::Modality::Video)
        );
        assert!(
            capabilities
                .media
                .contains_key(&cookie_agent_models::MediaKind::Video)
        );
    }
}

#[test]
fn bedrock_video_projection_matches_pinned_oven_ceiling() {
    let mut provider = record("@ai-sdk/amazon-bedrock", None);
    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .modalities
        .input = vec!["text".into(), "video".into()];
    let compiled = DynamicCompiler::family_registry()
        .compile_managed("sha256:bedrock-video", &provider, None)
        .unwrap();
    let video = &compiled.models.values().next().unwrap().capabilities.media
        [&cookie_agent_models::MediaKind::Video];

    assert_eq!(video.max_bytes, 25 * 1024 * 1024);
    assert_eq!(video.max_count, 1);
    assert_eq!(
        video
            .mime_types
            .iter()
            .map(|value| value.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "video/3gpp",
            "video/mp4",
            "video/mpeg",
            "video/mpg",
            "video/quicktime",
            "video/webm",
            "video/wmv",
            "video/x-flv",
            "video/x-matroska",
        ])
    );
}

#[test]
fn non_video_model_behavior_fingerprint_is_stable() {
    let compiled = DynamicCompiler::family_registry()
        .compile_managed(
            "sha256:test",
            &record("@ai-sdk/openai-compatible", Some("https://example.com/v1")),
            None,
        )
        .unwrap();
    assert_eq!(
        compiled
            .models
            .values()
            .next()
            .unwrap()
            .behavior_fingerprint
            .as_str(),
        "3a320f167eb773e1b67c5c8177623733b955e2971de564faf0122edee29842ae"
    );
}

#[test]
fn authored_shape_selects_chat() {
    let authored = ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: None,
        auth_override: None,
        shape: Some(ManagedModelShape::Chat),
        cache: None,
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };
    let compiled = DynamicCompiler::family_registry()
        .compile_managed(
            "sha256:test",
            &record("@ai-sdk/openai", None),
            Some(&authored),
        )
        .unwrap();
    assert_eq!(
        compiled.models.values().next().unwrap().resolved_shape,
        "chat"
    );
}

#[test]
fn managed_provider_cache_must_match_resolved_adaptor() {
    let authored = ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: None,
        auth_override: None,
        shape: None,
        cache: Some(
            serde_json::from_value(serde_json::json!({"system":"1h"})).expect("provider cache"),
        ),
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };
    assert!(matches!(
        DynamicCompiler::family_registry().compile_managed(
            "sha256:test",
            &record("@ai-sdk/openai", None),
            Some(&authored),
        ),
        Err(DynamicCompileError::Cache(_))
    ));
}

#[test]
fn managed_cache_validation_precedes_model_availability_and_resolution_skips() {
    let authored = ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: None,
        auth_override: None,
        shape: None,
        cache: Some(
            serde_json::from_value(serde_json::json!({"mode":"implicit"}))
                .expect("provider cache envelope"),
        ),
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };

    let mut absent = record("@ai-sdk/openai", None);
    absent.models.values_mut().next().unwrap().record = None;
    let mut deprecated = record("@ai-sdk/openai", None);
    deprecated
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .status = CatalogModelStatus::Deprecated;
    let mut unresolved = record("@ai-sdk/openai", None);
    unresolved
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .shape = Some("future".into());

    for record in [absent, deprecated, unresolved] {
        let error = DynamicCompiler::family_registry()
            .compile_managed("sha256:test", &record, Some(&authored))
            .unwrap_err()
            .to_string();
        assert!(error.contains("implicit"), "{error}");
        assert!(error.contains("auto"), "{error}");
    }
}

#[test]
fn managed_responses_compaction_setting_derives_native_capability() {
    let mut provider = record("@ai-sdk/openai", None);
    provider.id = ProviderId::new("openai").unwrap();
    let model_id = provider.models.keys().next().unwrap().clone();
    let authored: ModelsDevProvider = toml::from_str(&format!(
        r#"shape = "responses"

[model_overrides."{model_id}"]
compaction = "openai-responses-compact"
"#
    ))
    .unwrap();
    let compiled = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &provider, Some(&authored))
        .unwrap();
    assert_eq!(
        compiled
            .models
            .values()
            .next()
            .unwrap()
            .capabilities
            .compaction,
        cookie_agent_models::CompactionCapability::Native
    );
}

#[test]
fn managed_compaction_setting_rejects_wrong_recipe_and_legacy_value() {
    let legacy = toml::from_str::<ModelsDevProvider>(
        r#"[model_overrides."model/1"]
compaction = "v1"
"#,
    )
    .expect_err("legacy value must fail");
    assert!(legacy.to_string().contains("adapter-specific"));

    let authored: ModelsDevProvider = toml::from_str(
        r#"shape = "chat"

[model_overrides."model/1"]
compaction = "openai-responses-compact"
"#,
    )
    .unwrap();
    let mut provider = record("@ai-sdk/openai", None);
    provider.id = ProviderId::new("openai").unwrap();
    assert!(
        DynamicCompiler::family_registry()
            .compile_managed("sha256:test", &provider, Some(&authored))
            .is_err()
    );
}

#[test]
fn unresolved_placeholder_requires_setup() {
    let compiled = DynamicCompiler::family_registry()
        .compile_managed(
            "sha256:test",
            &record(
                "@ai-sdk/openai-compatible",
                Some("https://${DATABRICKS_HOST}/v1"),
            ),
            None,
        )
        .unwrap();
    assert_eq!(
        compiled.models.values().next().unwrap().status,
        CompiledModelStatus::SetupUnavailable
    );
}

#[test]
fn unknown_nested_family_only_marks_that_model_unsupported() {
    let mut provider = record("@ai-sdk/openai-compatible", Some("https://example.com/v1"));
    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .provider = Some(CatalogModelProviderMetadata {
        npm: Some("unknown-package".into()),
        api: None,
        shape: None,
    });
    let compiled = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &provider, None)
        .unwrap();
    assert!(compiled.models.is_empty());
    assert_eq!(
        compiled.unsupported_models[0].reason,
        "no_known_protocol_family"
    );
}

#[test]
fn mixed_family_nested_models_map_auth_and_route_adapters() {
    let mut azure = record("@ai-sdk/azure", None);
    azure
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .provider = Some(CatalogModelProviderMetadata {
        npm: Some("@ai-sdk/anthropic".into()),
        api: Some("https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1".into()),
        shape: None,
    });
    let azure_authored = ModelsDevProvider {
        base_url: None,
        setup: setup(&[("resource_name", "example")]),
        api_key: None,
        auth_override: Some(auth("azure-api-key-v1", &[("api_key", "secret")])),
        shape: None,
        cache: None,
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };
    let azure_model = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &azure, Some(&azure_authored))
        .unwrap()
        .models
        .into_values()
        .next()
        .unwrap();
    assert_eq!(azure_model.adapter, OvenAdapterFamily::AnthropicCompatible);
    assert_eq!(azure_model.auth.method, "anthropic-api-key-v1");
    assert_eq!(azure_model.effective_npm, "@ai-sdk/anthropic");

    let mut vertex = record("@ai-sdk/google-vertex", None);
    vertex.models.values_mut().next().unwrap().record.as_mut().unwrap().provider = Some(
        CatalogModelProviderMetadata {
            npm: Some("@ai-sdk/openai-compatible".into()),
            api: Some("https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/endpoints/openapi".into()),
            shape: None,
        },
    );
    let vertex_authored = ModelsDevProvider {
        base_url: None,
        setup: setup(&[
            ("endpoint", "us-central1-aiplatform.googleapis.com"),
            ("project", "example-project"),
            ("location", "us-central1"),
        ]),
        api_key: None,
        auth_override: Some(auth("oauth-access-token-v1", &[("access_token", "token")])),
        shape: None,
        cache: None,
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };
    let vertex_model = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &vertex, Some(&vertex_authored))
        .unwrap()
        .models
        .into_values()
        .next()
        .unwrap();
    assert_eq!(vertex_model.adapter, OvenAdapterFamily::OpenaiCompatible);
    assert_eq!(vertex_model.auth.method, "bearer-api-key-v1");

    let mut bedrock = record("@ai-sdk/amazon-bedrock", None);
    bedrock
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .provider = Some(CatalogModelProviderMetadata {
        npm: Some("@ai-sdk/amazon-bedrock/mantle".into()),
        api: Some("https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1".into()),
        shape: Some("responses".into()),
    });
    let bedrock_authored = ModelsDevProvider {
        base_url: None,
        setup: setup(&[("region", "us-east-1")]),
        api_key: None,
        auth_override: Some(auth("bearer-api-key-v1", &[("api_key", "bedrock-key")])),
        shape: None,
        cache: None,
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };
    let bedrock_model = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &bedrock, Some(&bedrock_authored))
        .unwrap()
        .models
        .into_values()
        .next()
        .unwrap();
    assert_eq!(bedrock_model.adapter, OvenAdapterFamily::OpenaiResponses);
    assert_eq!(bedrock_model.resolved_shape, "responses");
    assert_eq!(bedrock_model.effective_npm, "@ai-sdk/amazon-bedrock/mantle");
    assert_eq!(bedrock_model.auth.method, "bearer-api-key-v1");
}

#[test]
fn anthropic_compatible_and_bedrock_accept_toggle_and_budget_variants() {
    for npm in ["@ai-sdk/anthropic", "@ai-sdk/amazon-bedrock"] {
        let mut provider = record(npm, None);
        let model = provider
            .models
            .values_mut()
            .next()
            .unwrap()
            .record
            .as_mut()
            .unwrap();
        model.reasoning_options = vec![
            CatalogReasoningOption::Toggle,
            CatalogReasoningOption::BudgetTokens {
                min: Some(1024),
                max: Some(4096),
            },
        ];
        let compiled = DynamicCompiler::family_registry()
            .compile_managed("sha256:test", &provider, None)
            .unwrap();
        assert!(compiled.unsupported_models.is_empty());
        let model = compiled.models.values().next().unwrap();
        assert_eq!(model.variants.len(), 3);
        assert!(!model.variants.keys().any(|id| id.as_str() == "on"));
    }
}

#[test]
fn managed_effort_variant_order_preserves_catalog_value_order() {
    let mut provider = record("@ai-sdk/anthropic", None);
    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .reasoning_options = vec![CatalogReasoningOption::Effort {
        values: vec![Some("low".into()), Some("high".into()), Some("max".into())],
    }];

    let compiled = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &provider, None)
        .unwrap();
    let order = &compiled.models.values().next().unwrap().variant_order;

    assert_eq!(
        order.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
        ["low", "high", "max"]
    );
}

#[test]
fn managed_toggle_only_preserves_on_but_toggle_with_effort_suppresses_it() {
    let mut provider = record("@ai-sdk/anthropic", None);
    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .reasoning_options = vec![CatalogReasoningOption::Toggle];
    let toggle = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &provider, None)
        .unwrap();
    assert_eq!(
        toggle
            .models
            .values()
            .next()
            .unwrap()
            .variant_order
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["off", "on"]
    );

    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .reasoning_options
        .push(CatalogReasoningOption::Effort {
            values: vec![Some("low".into()), Some("high".into())],
        });
    let mixed = DynamicCompiler::family_registry()
        .compile_managed("sha256:test", &provider, None)
        .unwrap();
    assert_eq!(
        mixed
            .models
            .values()
            .next()
            .unwrap()
            .variant_order
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["off", "low", "high"]
    );
    assert!(
        !mixed
            .models
            .values()
            .next()
            .unwrap()
            .variants
            .keys()
            .any(|id| id.as_str() == "on")
    );
}

#[test]
fn custom_variant_order_uses_config_key_order() {
    let definition: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "bearer-api-key-v1", values = { api_key = "secret" } }

[models.test]
display_name = "Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {} }
variants = { zeta = { operation = "add" }, alpha = { operation = "add" } }
"#,
    )
    .unwrap();
    let ProviderDefinition::Custom(provider) = definition else {
        unreachable!();
    };
    let compiled = DynamicCompiler::family_registry()
        .compile_custom(&ProviderId::new("custom.test").unwrap(), &provider)
        .unwrap();

    assert_eq!(
        compiled
            .models
            .values()
            .next()
            .unwrap()
            .variant_order
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
}

fn header_map(values: &[(&str, &str)]) -> BTreeMap<HeaderName, SafeStaticHeaderValue> {
    values
        .iter()
        .map(|(name, value)| {
            (
                HeaderName::new(*name).unwrap(),
                SafeStaticHeaderValue::new(*value).unwrap(),
            )
        })
        .collect()
}

#[test]
fn headers_merge_case_insensitively_with_variant_precedence_and_deletion() {
    let definition: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }
headers = { X-Level = "provider", X-Delete = "" }

[models.test]
display_name = "Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {} }
headers = { x-LEVEL = "model", x-provider-only = "model" }
variants = { fast = { operation = "add", headers = { X-Level = "variant", X-Keep = "" } } }
"#,
    )
    .unwrap();
    let ProviderDefinition::Custom(provider) = definition else {
        unreachable!();
    };
    let global = header_map(&[
        ("x-level", "global"),
        ("x-delete", "global"),
        ("x-keep", "global"),
    ]);
    let compiled = DynamicCompiler::family_registry()
        .compile_custom_with_headers(&ProviderId::new("custom.test").unwrap(), &provider, &global)
        .unwrap();
    let model = &compiled.models[&ProviderModelId::new("test").unwrap()];
    assert_eq!(
        model.headers[&HeaderName::new("X-Level").unwrap()].as_str(),
        "model"
    );
    assert!(
        !model
            .headers
            .contains_key(&HeaderName::new("x-delete").unwrap())
    );
    assert_eq!(
        model.variants[&cookie_agent_identity::VariantId::new("fast").unwrap()].headers
            [&HeaderName::new("x-level").unwrap()]
            .as_str(),
        "variant"
    );
    assert!(
        !model.variants[&cookie_agent_identity::VariantId::new("fast").unwrap()]
            .headers
            .contains_key(&HeaderName::new("x-keep").unwrap())
    );
}

#[test]
fn auth_headers_are_allowed_while_transport_and_protocol_headers_are_rejected() {
    let definition: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "bearer-api-key-v1", values = { api_key = "typed" } }
headers = { Authorization = "Bearer configured", Cookie = "route=one", user-agent = "custom" }
[models.test]
display_name = "Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {} }
"#,
    )
    .unwrap();
    assert!(
        definition
            .validate_for(&ProviderId::new("custom.test").unwrap())
            .is_ok()
    );

    for forbidden in ["host", "content-type", "anthropic-beta", "x-amz-date"] {
        let global = header_map(&[(forbidden, "value")]);
        let ProviderDefinition::Custom(provider) = &definition else {
            unreachable!();
        };
        assert!(matches!(
            DynamicCompiler::family_registry().compile_custom_with_headers(
                &ProviderId::new("custom.test").unwrap(),
                provider,
                &global,
            ),
            Err(DynamicCompileError::StaticHeaders(_))
        ));
    }
}

#[test]
fn managed_provider_accepts_auth_owned_and_user_agent_headers() {
    let authored = ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: Some(SecretString::new("typed-secret").unwrap()),
        auth_override: None,
        shape: None,
        cache: None,
        headers: header_map(&[
            ("authorization", "Bearer configured"),
            ("user-agent", "managed-client"),
        ]),
        model_overrides: BTreeMap::new(),
    };
    assert!(
        ProviderDefinition::ModelsDev(authored.clone())
            .validate_for(&ProviderId::new("openai").unwrap())
            .is_ok()
    );
    let compiled = DynamicCompiler::family_registry()
        .compile_managed(
            "sha256:test",
            &record("@ai-sdk/openai", None),
            Some(&authored),
        )
        .unwrap();
    let model = compiled.models.values().next().unwrap();
    assert_eq!(
        model.headers[&HeaderName::new("authorization").unwrap()].as_str(),
        "Bearer configured"
    );
}

#[test]
fn merged_header_count_is_bounded() {
    let definition: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }
[models.test]
display_name = "Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {} }
"#,
    )
    .unwrap();
    let ProviderDefinition::Custom(mut provider) = definition else {
        unreachable!();
    };
    let global = (0..40)
        .map(|index| {
            (
                HeaderName::new(format!("x-global-{index}")).unwrap(),
                SafeStaticHeaderValue::new("value").unwrap(),
            )
        })
        .collect();
    provider.headers = (0..30)
        .map(|index| {
            (
                HeaderName::new(format!("x-provider-{index}")).unwrap(),
                SafeStaticHeaderValue::new("value").unwrap(),
            )
        })
        .collect();
    assert!(matches!(
        DynamicCompiler::family_registry().compile_custom_with_headers(
            &ProviderId::new("custom.test").unwrap(),
            &provider,
            &global,
        ),
        Err(DynamicCompileError::StaticHeaders(_))
    ));
}

#[test]
fn header_fingerprints_use_templates_not_environment_values() {
    let definition: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }
headers = { x-env = "${env:COOKIE_AGENT_FINGERPRINT_TEST:-fallback}" }
[models.test]
display_name = "Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {} }
"#,
    )
    .unwrap();
    let ProviderDefinition::Custom(provider) = definition else {
        unreachable!();
    };
    unsafe { std::env::set_var("COOKIE_AGENT_FINGERPRINT_TEST", "first") };
    let first = DynamicCompiler::family_registry()
        .compile_custom(&ProviderId::new("custom.test").unwrap(), &provider)
        .unwrap();
    unsafe { std::env::set_var("COOKIE_AGENT_FINGERPRINT_TEST", "second") };
    let second = DynamicCompiler::family_registry()
        .compile_custom(&ProviderId::new("custom.test").unwrap(), &provider)
        .unwrap();
    unsafe { std::env::remove_var("COOKIE_AGENT_FINGERPRINT_TEST") };
    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(
        first.models[&ProviderModelId::new("test").unwrap()].behavior_fingerprint,
        second.models[&ProviderModelId::new("test").unwrap()].behavior_fingerprint
    );
}
