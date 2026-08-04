use std::collections::BTreeMap;

use cookie_agent_identity::ProviderId;
use cookie_agent_models::{
    Catalog, CatalogReasoningOption, MODELS_DEV_ARTIFACT_BYTES, MODELS_DEV_ARTIFACT_SHA256,
    ModelsDevProvider, ProviderDefinition, build_model_set,
};
use sha2::{Digest as _, Sha256};

#[test]
fn embedded_catalog_has_exact_provenance_and_strict_reasoning_metadata() {
    let bytes = include_bytes!("../catalog/models-dev.json");
    assert_eq!(bytes.len(), MODELS_DEV_ARTIFACT_BYTES);
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        MODELS_DEV_ARTIFACT_SHA256
    );
    let catalog = Catalog::embedded().unwrap();
    let model = catalog.model("openai", "gpt-5.6-sol").unwrap();
    assert!(matches!(
        model.reasoning_options.as_slice(),
        [CatalogReasoningOption::Effort { .. }]
    ));
}

#[test]
fn included_models_dev_model_generates_only_declared_effort_variants() {
    let provider: ModelsDevProvider = toml::from_str(&format!(
        r#"
catalog_revision = "sha256:{MODELS_DEV_ARTIFACT_SHA256}"
auth = {{ type = "credential_store" }}
[models."gpt-5.6-sol"]
default_variant = "high"
"#
    ))
    .unwrap();
    let providers = BTreeMap::from([(
        ProviderId::new("openai").unwrap(),
        ProviderDefinition::ModelsDev(provider),
    )]);
    let set = build_model_set(&providers, &Catalog::embedded().unwrap(), None).unwrap();
    let entry = set.entries().next().unwrap().1;
    assert!(!entry.is_available());
    assert_eq!(
        entry
            .variants()
            .keys()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["high", "low", "max", "medium", "none", "xhigh"]
    );
    assert_eq!(entry.default_variant().unwrap().as_str(), "high");
}

#[test]
fn budget_toggle_and_google_effort_options_generate_deterministic_unions() {
    let catalog = Catalog::embedded().unwrap();
    for (model, expected) in [
        (
            "gemini-2.5-flash",
            vec!["budget-max", "budget-min", "off", "on"],
        ),
        ("gemini-2.5-pro", vec!["budget-max", "budget-min"]),
        (
            "gemini-3-flash-preview",
            vec!["high", "low", "medium", "minimal"],
        ),
    ] {
        let provider: ModelsDevProvider = toml::from_str(&format!(
            r#"
catalog_revision = "sha256:{MODELS_DEV_ARTIFACT_SHA256}"
auth = {{ type = "credential_store" }}
[models."{model}"]
"#
        ))
        .unwrap();
        let providers = BTreeMap::from([(
            ProviderId::new("google").unwrap(),
            ProviderDefinition::ModelsDev(provider),
        )]);
        let set = build_model_set(&providers, &catalog, None)
            .unwrap_or_else(|error| panic!("{model}: {error}"));
        assert_eq!(
            set.entries()
                .next()
                .unwrap()
                .1
                .variants()
                .keys()
                .map(|id| id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}
