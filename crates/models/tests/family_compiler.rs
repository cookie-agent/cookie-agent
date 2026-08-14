use std::collections::BTreeMap;

use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, ProviderId, ProviderModelId, SetupFieldId,
};
use cookie_agent_models::{
    AuthOverride, BoundedSetupString, ManagedModelShape, ModelsDevProvider, ProviderDefinition,
    SafeSetupValue, SecretString,
    adapters::OvenAdapterFamily,
    catalog::{
        CatalogInterleaved, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelProviderMetadata, CatalogModelRecord, CatalogModelStatus,
        CatalogProviderRecord, CatalogReasoningOption,
    },
    compiler::{CompiledModelStatus, DynamicCompiler},
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
fn authored_shape_selects_chat() {
    let authored = ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: None,
        auth_override: None,
        shape: Some(ManagedModelShape::Chat),
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
