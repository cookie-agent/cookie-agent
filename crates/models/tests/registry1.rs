use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    os::unix::fs::PermissionsExt as _,
    sync::{Arc, Mutex},
};

use cookie_agent_models::recipes::{
    CatalogModelClaimInput, CatalogProviderClaimInput, ClaimPresence, ModelRecipeMatch,
    ProviderRecipeMatch, RecipeQuarantineReason, auth_method, registry1, route_openai_model,
};
use cookie_agent_models::{
    adapters::{OvenAdapterFamily, wire_adapter_for_recipe},
    catalog::{
        CatalogClaim as RawCatalogClaim, CatalogRequest, CatalogTransport, CatalogTransportError,
        CatalogTransportFuture, CatalogTransportResponse,
    },
    secure_store::SecureDirectory,
};

#[derive(Clone)]
struct FailingTransport(
    Arc<Mutex<VecDeque<Result<CatalogTransportResponse, CatalogTransportError>>>>,
);

impl Default for FailingTransport {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::from([Err(
            CatalogTransportError::RequestFailed,
        )]))))
    }
}

impl FailingTransport {
    fn responding(response: CatalogTransportResponse) -> Self {
        Self(Arc::new(Mutex::new(VecDeque::from([Ok(response)]))))
    }
}

impl CatalogTransport for FailingTransport {
    fn fetch(&self, _: CatalogRequest) -> CatalogTransportFuture<'_> {
        let response = self.0.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { response })
    }
}

fn claim<'a>(
    id: &'a str,
    npm: Option<&'a str>,
    api: Option<&'a str>,
    env: &'a [String],
) -> CatalogProviderClaimInput<'a> {
    CatalogProviderClaimInput {
        id,
        npm: npm.map_or(RawCatalogClaim::Absent, RawCatalogClaim::Present),
        api: api.map_or(RawCatalogClaim::Absent, RawCatalogClaim::Present),
        environment: RawCatalogClaim::Present(env),
        shape: RawCatalogClaim::Absent,
    }
}

#[test]
fn registry1_contains_only_the_exact_reviewed_entries() {
    let registry = registry1();
    assert_eq!(registry.schema_version(), 1);
    assert_eq!(registry.recipes().len(), 13);
    assert_eq!(registry.provider_recipes("openai").len(), 2);
    assert!(registry.provider_recipes("mistral").is_empty());

    let exact = [
        (
            "anthropic",
            "@ai-sdk/anthropic",
            None,
            vec!["ANTHROPIC_API_KEY"],
        ),
        ("openai", "@ai-sdk/openai", None, vec!["OPENAI_API_KEY"]),
        (
            "openrouter",
            "@openrouter/ai-sdk-provider",
            Some("https://openrouter.ai/api/v1"),
            vec!["OPENROUTER_API_KEY"],
        ),
        (
            "google",
            "@ai-sdk/google",
            None,
            vec![
                "GOOGLE_API_KEY",
                "GOOGLE_GENERATIVE_AI_API_KEY",
                "GEMINI_API_KEY",
            ],
        ),
        ("cohere", "@ai-sdk/cohere", None, vec!["COHERE_API_KEY"]),
        ("groq", "@ai-sdk/groq", None, vec!["GROQ_API_KEY"]),
        (
            "togetherai",
            "@ai-sdk/togetherai",
            None,
            vec!["TOGETHER_API_KEY"],
        ),
        (
            "deepinfra",
            "@ai-sdk/deepinfra",
            None,
            vec!["DEEPINFRA_API_KEY"],
        ),
        (
            "fireworks-ai",
            "@ai-sdk/openai-compatible",
            Some("https://api.fireworks.ai/inference/v1/"),
            vec!["FIREWORKS_API_KEY"],
        ),
        (
            "google-vertex",
            "@ai-sdk/google-vertex",
            None,
            vec![
                "GOOGLE_VERTEX_PROJECT",
                "GOOGLE_VERTEX_LOCATION",
                "GOOGLE_APPLICATION_CREDENTIALS",
            ],
        ),
        (
            "amazon-bedrock",
            "@ai-sdk/amazon-bedrock",
            None,
            vec![
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_REGION",
                "AWS_BEARER_TOKEN_BEDROCK",
            ],
        ),
        (
            "azure",
            "@ai-sdk/azure",
            None,
            vec!["AZURE_RESOURCE_NAME", "AZURE_API_KEY"],
        ),
    ];
    for (id, npm, api, env) in exact {
        let env = env.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert!(
            matches!(
                registry.match_provider(&claim(id, Some(npm), api, &env)),
                ProviderRecipeMatch::Supported(_)
            ),
            "{id}"
        );
    }
}

#[test]
fn provider_claims_are_presence_sensitive_and_environment_is_an_exact_set() {
    let registry = registry1();
    let env = vec!["FIREWORKS_API_KEY".to_owned()];
    let trailing_slash_missing = claim(
        "fireworks-ai",
        Some("@ai-sdk/openai-compatible"),
        Some("https://api.fireworks.ai/inference/v1"),
        &env,
    );
    assert_eq!(
        registry.match_provider(&trailing_slash_missing),
        ProviderRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogProviderApiDrift)
    );

    let groq_env = vec!["GROQ_API_KEY".to_owned()];
    let missing_npm = CatalogProviderClaimInput {
        id: "groq",
        npm: RawCatalogClaim::Absent,
        api: RawCatalogClaim::Absent,
        environment: RawCatalogClaim::Present(&groq_env),
        shape: RawCatalogClaim::Absent,
    };
    assert_eq!(
        registry.match_provider(&missing_npm),
        ProviderRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogProviderNpmDrift)
    );

    let unexpected_shape = CatalogProviderClaimInput {
        id: "groq",
        npm: RawCatalogClaim::Present("@ai-sdk/groq"),
        api: RawCatalogClaim::Absent,
        environment: RawCatalogClaim::Present(&groq_env),
        shape: RawCatalogClaim::Present("responses"),
    };
    assert_eq!(
        registry.match_provider(&unexpected_shape),
        ProviderRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogProviderShapeDrift)
    );

    let duplicate_env = vec!["GROQ_API_KEY".to_owned(), "GROQ_API_KEY".to_owned()];
    assert_eq!(
        registry.match_provider(&claim("groq", Some("@ai-sdk/groq"), None, &duplicate_env,)),
        ProviderRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogProviderEnvDrift)
    );

    let unexpected_api = vec!["OPENAI_API_KEY".to_owned()];
    assert_eq!(
        registry.match_provider(&claim(
            "openai",
            Some("@ai-sdk/openai"),
            Some("https://api.openai.com/v1"),
            &unexpected_api,
        )),
        ProviderRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogProviderApiDrift)
    );
}

#[test]
fn openai_family_routing_is_case_sensitive_and_bounded_by_suffix() {
    for id in ["gpt-5", "gpt-5-mini", "o1", "o3-pro", "o4-mini"] {
        assert_eq!(route_openai_model(id), Ok("responses"), "{id}");
    }
    for id in ["gpt-4.1", "gpt-4.1-mini", "gpt-4o", "gpt-3.5-turbo-0125"] {
        assert_eq!(route_openai_model(id), Ok("chat"), "{id}");
    }
    for id in ["GPT-5", "gpt-50", "gpt-4", "text-embedding-3-large"] {
        assert_eq!(
            route_openai_model(id),
            Err(RecipeQuarantineReason::UnreviewedOpenaiModelFamily),
            "{id}"
        );
    }
}

#[test]
fn provider_recipe_to_wire_adapter_mapping_is_strict_and_attribution_stable() {
    let cases = [
        (
            "anthropic.messages.v1",
            "claude",
            "oven.anthropic.messages",
            OvenAdapterFamily::Anthropic,
        ),
        (
            "openai.responses.v1",
            "gpt-5",
            "oven.openai.responses",
            OvenAdapterFamily::OpenaiResponses,
        ),
        (
            "openai.chat.v1",
            "gpt-4o",
            "oven.openai.chat",
            OvenAdapterFamily::OpenaiChat,
        ),
        (
            "openrouter.chat.v1",
            "org/model",
            "oven.openai-compatible.chat",
            OvenAdapterFamily::OpenaiCompatible,
        ),
        (
            "google.gemini.v1",
            "gemini-2.5-pro",
            "oven.google.gemini.generate-content",
            OvenAdapterFamily::GoogleGemini,
        ),
        (
            "cohere.chat.v2",
            "command-r",
            "oven.cohere.chat-v2",
            OvenAdapterFamily::CohereV2Chat,
        ),
        (
            "cohere.chat.v2",
            "north-mini-code-1-0",
            "oven.openai-compatible.chat",
            OvenAdapterFamily::OpenaiCompatible,
        ),
        (
            "google.vertex.gemini.v1",
            "gemini-2.5-pro",
            "oven.google.vertex.generate-content",
            OvenAdapterFamily::GoogleVertexGemini,
        ),
        (
            "amazon.bedrock.converse.v1",
            "anthropic.claude",
            "oven.bedrock.converse",
            OvenAdapterFamily::AwsBedrockConverse,
        ),
        (
            "azure.openai.v1",
            "gpt-5",
            "oven.azure.openai.responses",
            OvenAdapterFamily::AzureOpenaiResponses,
        ),
        (
            "azure.openai.v1",
            "gpt-4o",
            "oven.azure.openai.chat",
            OvenAdapterFamily::AzureOpenaiChat,
        ),
    ];
    for (recipe, model, adapter_recipe, family) in cases {
        let mapping = wire_adapter_for_recipe(recipe, model).unwrap();
        assert_eq!(mapping.provider_recipe_id, recipe);
        assert_eq!(mapping.adapter_recipe_id, adapter_recipe);
        assert_eq!(mapping.family, family);
    }
    assert_eq!(
        wire_adapter_for_recipe("future.recipe.v1", "model"),
        Err(RecipeQuarantineReason::NoReviewedProviderRecipe)
    );
}

#[test]
fn current_auth_registry_has_exact_wire_families_and_no_future_ambient_methods() {
    let current = [
        "no-auth-v1",
        "bearer-api-key-v1",
        "api-key-header-v1",
        "anthropic-api-key-v1",
        "google-api-key-header-v1",
        "oauth-access-token-v1",
        "aws-sigv4-credentials-v1",
        "azure-api-key-v1",
    ];
    for id in current {
        assert!(auth_method(id).is_some(), "{id}");
    }
    for future in [
        "basic-v1",
        "google-adc-v1",
        "aws-sdk-v1",
        "azure-entra-v1",
        "bedrock-bearer-v1",
    ] {
        assert!(auth_method(future).is_none(), "{future}");
    }
}

#[test]
fn environment_alias_mapping_is_explicit_and_uses_any_of_only_for_one_semantic_field() {
    let registry = registry1();
    let google = registry.recipe("google.gemini.v1").unwrap();
    assert_eq!(
        google.credential_environment_aliases("api_key"),
        [
            "GOOGLE_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GEMINI_API_KEY"
        ]
    );
    let vertex = registry.recipe("google.vertex.gemini.v1").unwrap();
    assert!(
        vertex
            .credential_environment_aliases("access_token")
            .is_empty()
    );
    let bedrock = registry.recipe("amazon.bedrock.converse.v1").unwrap();
    assert_eq!(
        bedrock.credential_environment_aliases("access_key_id"),
        ["AWS_ACCESS_KEY_ID"]
    );
    assert!(
        bedrock
            .credential_environment_aliases("session_token")
            .is_empty()
    );
}

#[test]
fn claim_presence_supports_absence_and_reviewed_one_of_without_normalizing_raw_values() {
    assert!(ClaimPresence::Absent.matches(None));
    assert!(!ClaimPresence::Absent.matches(Some("")));
    assert!(ClaimPresence::PresentOneOf(&["a", "b"]).matches(Some("b")));
    assert!(!ClaimPresence::PresentOneOf(&["a", "b"]).matches(Some("B")));
}

#[tokio::test]
async fn bundled_fixture_matches_every_registry1_claim_without_typed_drift() {
    let temporary = tempfile::tempdir().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory = SecureDirectory::open_in(temporary.path(), "catalog").unwrap();
    let snapshot =
        cookie_agent_models::catalog::CatalogManager::new(FailingTransport::default(), directory)
            .refresh_at("2026-08-05T00:00:00Z".parse().unwrap())
            .await
            .unwrap();
    assert_snapshot_conforms(&snapshot);
}

#[tokio::test]
async fn approved_full_live_fixture_accounts_for_every_registry1_claim_input() {
    let metadata: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/models-dev-live-audit-2026-08-05.meta.json"
    ))
    .unwrap();
    assert_eq!(metadata["review_status"], "approved");
    assert_eq!(metadata["runtime_pin"], false);
    assert_eq!(metadata["test_only"], true);

    let temporary = tempfile::tempdir().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory = SecureDirectory::open_in(temporary.path(), "catalog").unwrap();
    let bytes = include_bytes!("fixtures/models-dev-live-audit-2026-08-05.json").to_vec();
    let snapshot = cookie_agent_models::catalog::CatalogManager::new(
        FailingTransport::responding(CatalogTransportResponse::from_bytes(200, bytes)),
        directory,
    )
    .refresh_at(metadata["captured_at"].as_str().unwrap().parse().unwrap())
    .await
    .unwrap();

    let registry = registry1();
    let mut provider_classes = ClassificationCounts::default();
    let mut model_classes = ClassificationCounts::default();
    let mut provider_models = 0_u64;
    for (provider_id, provider) in &snapshot.providers {
        let Some(record) = provider.record.as_ref() else {
            provider_classes.quarantined += 1;
            continue;
        };
        let input = CatalogProviderClaimInput::from_record(record);
        assert_provider_claim_input_exact(&input, record);
        match registry.match_provider(&input) {
            ProviderRecipeMatch::Supported(_) => provider_classes.supported += 1,
            ProviderRecipeMatch::Unsupported(_) => provider_classes.unsupported += 1,
            ProviderRecipeMatch::Quarantined(_) => provider_classes.quarantined += 1,
        }

        for (table_id, model) in &record.models {
            provider_models += 1;
            let Some(model) = model.record.as_ref() else {
                model_classes.quarantined += 1;
                continue;
            };
            let input = CatalogModelClaimInput::from_record(table_id.as_str(), model);
            assert_eq!(input.table_key, table_id.as_str());
            assert!(std::ptr::eq(input.record, model));
            match registry.match_model(provider_id.as_str(), &input) {
                ModelRecipeMatch::Supported(_) => model_classes.supported += 1,
                ModelRecipeMatch::Omitted => model_classes.unsupported += 1,
                ModelRecipeMatch::Quarantined(_) => model_classes.quarantined += 1,
            }
        }
    }

    assert_eq!(
        snapshot.providers.len() as u64,
        metadata["provider_count"].as_u64().unwrap()
    );
    assert_eq!(provider_classes.total(), snapshot.providers.len() as u64);
    assert_eq!(
        provider_models,
        metadata["provider_model_count"].as_u64().unwrap()
    );
    assert_eq!(model_classes.total(), provider_models);
    assert_eq!(
        (
            provider_classes.supported,
            provider_classes.unsupported,
            provider_classes.quarantined,
        ),
        (12, 168, 0)
    );
    assert_eq!(
        (
            model_classes.supported,
            model_classes.unsupported,
            model_classes.quarantined,
        ),
        (624, 350, 5_157)
    );

    let canonical_quarantine = snapshot
        .quarantine
        .iter()
        .filter_map(|entry| entry.canonical_model_id.as_deref())
        .collect::<BTreeSet<_>>();
    assert!(canonical_quarantine.is_empty());
    for (table_id, record) in &snapshot.canonical_models {
        assert_eq!(table_id, &record.id);
        assert!(!record.name.is_empty());
        assert!(!record.description.is_empty());
        assert!(record.metadata_digest.starts_with("sha256:"));
        assert_eq!(record.metadata_digest.len(), 71);
    }
    assert_eq!(
        snapshot.canonical_models.len() as u64 + canonical_quarantine.len() as u64,
        metadata["canonical_model_count"].as_u64().unwrap()
    );
}

#[derive(Default)]
struct ClassificationCounts {
    supported: u64,
    unsupported: u64,
    quarantined: u64,
}

impl ClassificationCounts {
    const fn total(&self) -> u64 {
        self.supported + self.unsupported + self.quarantined
    }
}

fn assert_provider_claim_input_exact(
    input: &CatalogProviderClaimInput<'_>,
    record: &cookie_agent_models::catalog::CatalogProviderRecord,
) {
    assert_eq!(input.id, record.id.as_str());
    assert_string_claim_exact(&input.npm, &record.claims.npm);
    assert_string_claim_exact(&input.api, &record.claims.api);
    assert_string_claim_exact(&input.shape, &record.claims.shape);
    match (&input.environment, &record.claims.environment) {
        (RawCatalogClaim::Absent, RawCatalogClaim::Absent) => {}
        (RawCatalogClaim::Present(input), RawCatalogClaim::Present(record)) => {
            assert_eq!(*input, record.as_slice());
        }
        _ => panic!("provider environment claim presence changed"),
    }
}

fn assert_string_claim_exact(input: &RawCatalogClaim<&str>, record: &RawCatalogClaim<String>) {
    match (input, record) {
        (RawCatalogClaim::Absent, RawCatalogClaim::Absent) => {}
        (RawCatalogClaim::Present(input), RawCatalogClaim::Present(record)) => {
            assert_eq!(*input, record);
        }
        _ => panic!("provider string claim presence changed"),
    }
}

#[tokio::test]
async fn synthetic_claim_fixture_exercises_presence_and_shape_drift() {
    let temporary = tempfile::tempdir().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory = SecureDirectory::open_in(temporary.path(), "catalog").unwrap();
    let bytes = include_bytes!("fixtures/models-dev-claims-synthetic.json").to_vec();
    let snapshot = cookie_agent_models::catalog::CatalogManager::new(
        FailingTransport::responding(CatalogTransportResponse::from_bytes(200, bytes)),
        directory,
    )
    .refresh_at("2026-08-05T00:00:00Z".parse().unwrap())
    .await
    .unwrap();
    let registry = registry1();
    let openai_id = cookie_agent_identity::ProviderId::new("openai").unwrap();
    let openai = snapshot
        .provider(&openai_id)
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert!(matches!(
        registry.match_provider(&CatalogProviderClaimInput::from_record(openai)),
        ProviderRecipeMatch::Supported(_)
    ));
    let drift = openai
        .models
        .get(&cookie_agent_identity::ProviderModelId::new("compat/audit").unwrap())
        .unwrap()
        .record
        .as_ref()
        .unwrap();
    assert_eq!(
        registry.match_model(
            "openai",
            &CatalogModelClaimInput::from_record("compat/audit", drift),
        ),
        ModelRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogModelShapeDrift)
    );
}

fn assert_snapshot_conforms(snapshot: &cookie_agent_models::catalog::CatalogSnapshot) {
    let registry = registry1();
    let providers = registry
        .recipes()
        .map(|recipe| recipe.provider_id)
        .collect::<BTreeSet<_>>();
    for provider_id in providers {
        let id = cookie_agent_identity::ProviderId::new(provider_id).unwrap();
        let record = snapshot.provider(&id).unwrap().record.as_ref().unwrap();
        assert!(
            matches!(
                registry.match_provider(&CatalogProviderClaimInput::from_record(record)),
                ProviderRecipeMatch::Supported(_)
            ),
            "{provider_id}"
        );
        for (table_id, model) in &record.models {
            let Some(model) = model.record.as_ref() else {
                continue;
            };
            if let ModelRecipeMatch::Quarantined(reason) = registry.match_model(
                provider_id,
                &CatalogModelClaimInput::from_record(table_id.as_str(), model),
            ) {
                assert!(
                    !matches!(
                        reason,
                        RecipeQuarantineReason::CatalogModelProviderNpmDrift
                            | RecipeQuarantineReason::CatalogModelProviderApiDrift
                            | RecipeQuarantineReason::CatalogModelProviderShapeDrift
                            | RecipeQuarantineReason::CatalogModelShapeDrift
                    ),
                    "{provider_id}/{table_id}: {}",
                    reason.code()
                );
            }
        }
    }
}
