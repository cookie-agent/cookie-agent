use std::collections::BTreeMap;

use cookie_agent_models::{
    Catalog, CatalogModelStatus, CatalogRecipe, MODELS_DEV_ARTIFACT_BYTES,
    MODELS_DEV_ARTIFACT_SHA256, MODELS_DEV_COMMIT, UnsupportedReason,
};
use oven_sdk::{CancellationCapability, Capability, CompactionCapability, ReplayCapability};
use sha2::{Digest as _, Sha256};

#[test]
fn embedded_catalog_matches_exact_provenance_and_retains_every_record() {
    let bytes = include_bytes!("../catalog/models-dev.json");
    assert_eq!(bytes.len(), MODELS_DEV_ARTIFACT_BYTES);
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        MODELS_DEV_ARTIFACT_SHA256
    );
    assert_eq!(MODELS_DEV_COMMIT.len(), 40);
    assert_ne!(bytes.last(), Some(&b'\n'));

    let catalog = Catalog::embedded().unwrap();
    assert_eq!(catalog.providers().len(), 177);
    assert_eq!(catalog.models().len(), 5_935);
    assert!(catalog.revision().starts_with("sha256:"));
    assert_eq!(catalog.revision().len(), 71);
    assert!(catalog.is_known_provider("anthropic"));
    assert!(!catalog.is_known_provider("not-upstream"));
    assert!(catalog.is_supported_provider("anthropic"));
    assert!(!catalog.is_supported_provider("amazon-bedrock"));

    let sorted = catalog.models().windows(2).all(|pair| {
        (&pair[0].provider_id, &pair[0].model_id) <= (&pair[1].provider_id, &pair[1].model_id)
    });
    assert!(sorted);
}

#[test]
fn canonical_mapping_is_exact_only_and_dates_and_limits_are_strict() {
    let catalog = Catalog::embedded().unwrap();
    let first_party = catalog.model("anthropic", "claude-opus-4-6").unwrap();
    assert_eq!(
        first_party.canonical_model_id.as_deref(),
        Some("anthropic/claude-opus-4-6")
    );
    let routed = catalog
        .model("openrouter", "anthropic/claude-opus-4.6")
        .unwrap();
    assert_eq!(routed.canonical_model_id, None);

    let mut invalid: serde_json::Value =
        serde_json::from_slice(include_bytes!("../catalog/models-dev.json")).unwrap();
    invalid["providers"]["anthropic"]["models"]["claude-opus-4-6"]["release_date"] =
        serde_json::json!("2025-02-30");
    assert!(Catalog::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
    invalid["providers"]["anthropic"]["models"]["claude-opus-4-6"]["release_date"] =
        serde_json::json!("2025-05-22");
    invalid["providers"]["anthropic"]["models"]["claude-opus-4-6"]["limit"]["output"] =
        serde_json::json!(-1);
    assert!(Catalog::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
}

#[test]
fn recipes_are_explicit_and_unsupported_packages_stay_known() {
    let catalog = Catalog::embedded().unwrap();
    assert_eq!(
        catalog.recipe(catalog.model("anthropic", "claude-opus-4-6").unwrap()),
        Ok(CatalogRecipe::Anthropic)
    );
    assert_eq!(
        catalog.recipe(catalog.model("openai", "gpt-5.4").unwrap()),
        Ok(CatalogRecipe::OpenAiResponses)
    );
    assert_eq!(
        catalog.recipe(catalog.model("openai", "gpt-4o").unwrap()),
        Ok(CatalogRecipe::OpenAiChat)
    );
    let bedrock = catalog
        .models()
        .iter()
        .find(|model| model.provider_id.contains("bedrock"))
        .unwrap();
    assert!(matches!(
        catalog.recipe(bedrock),
        Err(UnsupportedReason::ExplicitlyUnsupported | UnsupportedReason::UnreviewedPackage)
    ));
    let deprecated = catalog
        .models()
        .iter()
        .find(|model| model.status == CatalogModelStatus::Deprecated)
        .unwrap();
    assert_eq!(
        catalog.recipe(deprecated),
        Err(UnsupportedReason::DeprecatedModel)
    );
}

#[test]
fn generated_models_are_text_only_conservative_and_secret_free_in_debug() {
    let catalog = Catalog::embedded().unwrap();
    for (provider_id, model_id) in [
        ("anthropic", "claude-opus-4-6"),
        ("openai", "gpt-5.4"),
        ("google", "gemini-2.5-flash"),
        ("cohere", "command-a-03-2025"),
        ("openrouter", "anthropic/claude-opus-4.6"),
    ] {
        let provider = &catalog.providers()[provider_id];
        let credential = provider.credential_fields[0].clone();
        let model = catalog.model(provider_id, model_id).unwrap();
        if catalog.recipe(model).is_err() {
            continue;
        }
        let entry = catalog
            .build_generated(
                model,
                &BTreeMap::from([(credential, "catalog-super-secret".into())]),
            )
            .unwrap();
        let capabilities = &entry.descriptor().capabilities;
        assert_eq!(
            capabilities
                .modalities
                .input
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>(),
            ["text"]
        );
        assert_eq!(capabilities.cancellation, CancellationCapability::LocalOnly);
        assert_eq!(capabilities.compaction, CompactionCapability::Unsupported);
        assert_eq!(
            capabilities.replay.capability,
            ReplayCapability::Unsupported
        );
        assert!(!capabilities.features.contains(Capability::REASONING));
        assert!(!capabilities.features.contains(Capability::PARALLEL_TOOLS));
        assert!(
            !capabilities
                .features
                .contains(Capability::TOOL_INPUT_DELTAS)
        );
        assert_eq!(
            entry.defaults().max_output_tokens,
            Some(model.limits.output.min(16_384))
        );
        assert_eq!(entry.defaults().top_p, None);
        assert!(!format!("{entry:?}").contains("catalog-super-secret"));
    }
}
