use std::collections::BTreeMap;

use cookie_agent_identity::{ProviderId, ProviderModelId};
use cookie_agent_models::{
    adapters::OvenAdapterFamily,
    catalog::{
        CatalogInterleaved, CatalogLimits, CatalogModalities, CatalogModelRecord,
        CatalogModelStatus, CatalogProviderRecord,
    },
    recipes::{
        FamilyKind, ResolvedShape, family_registry, placeholders, resolve_model, setup_field_name,
        substitute_placeholders,
    },
};

fn provider(npm: &str, api: Option<&str>) -> CatalogProviderRecord {
    CatalogProviderRecord {
        id: ProviderId::new("example").unwrap(),
        name: "Example".into(),
        environment: vec!["EXAMPLE_API_KEY".into()],
        npm: npm.into(),
        api: api.map(str::to_owned),
        shape: None,
        documentation_url: "https://example.com/docs".into(),
        models: BTreeMap::new(),
    }
}

fn model() -> CatalogModelRecord {
    CatalogModelRecord {
        id: ProviderModelId::new("model-1").unwrap(),
        name: "Model".into(),
        description: "Model".into(),
        family: None,
        attachment: false,
        reasoning: true,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".into(),
        last_updated: "2026-01-01".into(),
        modalities: CatalogModalities {
            input: vec!["text".into(), "image".into()],
            output: vec!["text".into()],
        },
        limits: CatalogLimits {
            context: 128_000,
            input: None,
            output: 16_384,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        cost: None,
        interleaved: Some(CatalogInterleaved::ReasoningContent),
        canonical_provenance: None,
    }
}

#[test]
fn classifies_every_supported_npm_family() {
    let registry = family_registry();
    assert_eq!(registry.recipes().len(), 20);
    for recipe in registry.recipes() {
        assert_eq!(registry.by_npm(recipe.npm), Some(recipe));
    }
    assert!(registry.by_npm("@ai-sdk/vercel").is_none());
    assert!(registry.by_npm("future-unknown-provider").is_none());
}

#[test]
fn classifies_every_provider_in_the_bundled_catalog() {
    let catalog: serde_json::Value =
        serde_json::from_slice(cookie_agent_models::catalog::MODELS_DEV_BOOTSTRAP).unwrap();
    let providers = catalog["providers"].as_object().unwrap();
    let supported = providers
        .values()
        .filter(|provider| {
            provider["npm"]
                .as_str()
                .and_then(|npm| family_registry().by_npm(npm))
                .is_some()
        })
        .count();
    assert_eq!(supported, 170);
    for provider in providers.values() {
        let npm = provider["npm"].as_str().unwrap();
        let known = family_registry().by_npm(npm).is_some();
        if !known {
            assert!(matches!(
                npm,
                "@ai-sdk/vercel"
                    | "gitlab-ai-provider"
                    | "@aihubmix/ai-sdk-provider"
                    | "@jerome-benoit/sap-ai-provider-v2"
                    | "ai-gateway-provider"
                    | "merge-gateway-ai-sdk-provider"
                    | "@ai-sdk/gateway"
            ));
        }
    }
}

#[test]
fn catalog_endpoint_and_nested_overrides_are_authoritative() {
    let provider = provider(
        "@ai-sdk/azure",
        Some("https://${AZURE_RESOURCE_NAME}.openai.azure.com"),
    );
    let mut model = model();
    model.provider = Some(cookie_agent_models::catalog::CatalogModelProviderMetadata {
        npm: Some("@ai-sdk/anthropic".into()),
        api: Some("https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1".into()),
        shape: None,
    });
    let resolved = resolve_model(&provider, &model, None, None).unwrap();
    assert_eq!(resolved.recipe.family, FamilyKind::Anthropic);
    assert_eq!(resolved.adapter, OvenAdapterFamily::AnthropicCompatible);
    assert_eq!(
        resolved.endpoint_template.as_deref(),
        Some("https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1")
    );
}

#[test]
fn shape_overrides_route_openai_and_nested_responses() {
    let provider = provider("@ai-sdk/openai", None);
    let model = model();
    let default = resolve_model(&provider, &model, None, None).unwrap();
    assert_eq!(default.shape, ResolvedShape::Responses);
    assert_eq!(default.adapter, OvenAdapterFamily::OpenaiResponses);
    let chat = resolve_model(
        &provider,
        &model,
        Some(cookie_agent_models::ManagedModelShape::Chat),
        None,
    )
    .unwrap();
    assert_eq!(chat.adapter, OvenAdapterFamily::OpenaiChat);
}

#[test]
fn derives_and_substitutes_setup_placeholders() {
    let template = "https://${DATABRICKS_HOST}/serving/${AZURE_RESOURCE_NAME}/${AWS_REGION}";
    assert_eq!(
        placeholders(template),
        vec!["AWS_REGION", "AZURE_RESOURCE_NAME", "DATABRICKS_HOST"]
    );
    assert_eq!(setup_field_name("DATABRICKS_HOST"), "databricks_host");
    let values = BTreeMap::from([
        ("databricks_host".into(), "workspace.example.com".into()),
        ("resource_name".into(), "resource".into()),
        ("region".into(), "us-east-1".into()),
    ]);
    assert_eq!(
        substitute_placeholders(template, &values).as_deref(),
        Some("https://workspace.example.com/serving/resource/us-east-1")
    );
}
