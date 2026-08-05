use cookie_agent_identity::{ProviderId, ProviderModelId};
use cookie_agent_models::{
    ProviderDefinition,
    catalog::{
        CatalogClaim, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelProviderClaims, CatalogModelRecord, CatalogModelStatus, CatalogProviderClaims,
        CatalogProviderRecord,
    },
    compiler::{AuthSourceCategory, CompiledModelStatus, DynamicCompileError, DynamicCompiler},
    recipes::RecipeQuarantineReason,
};

fn model(id: &str) -> CatalogModelRecord {
    CatalogModelRecord {
        id: ProviderModelId::new(id).unwrap(),
        name: id.to_owned(),
        description: "test model".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 128_000,
            input: None,
            output: 16_384,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        canonical_provenance: None,
    }
}

fn provider(
    id: &str,
    npm: &str,
    api: Option<&str>,
    env: &[&str],
    models: impl IntoIterator<Item = CatalogModelRecord>,
) -> CatalogProviderRecord {
    let environment = env
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let api = api.map(str::to_owned);
    CatalogProviderRecord {
        id: ProviderId::new(id).unwrap(),
        name: id.to_owned(),
        environment: environment.clone(),
        npm: npm.to_owned(),
        api: api.clone(),
        shape: None,
        claims: CatalogProviderClaims {
            environment: CatalogClaim::Present(environment),
            npm: CatalogClaim::Present(npm.to_owned()),
            api: api.map_or(CatalogClaim::Absent, CatalogClaim::Present),
            shape: CatalogClaim::Absent,
        },
        documentation_url: "https://example.test/docs".to_owned(),
        models: models
            .into_iter()
            .map(|model| {
                (
                    model.id.clone(),
                    CatalogModelEntry {
                        id: model.id.clone(),
                        record: Some(model),
                        quarantine: None,
                    },
                )
            })
            .collect(),
    }
}

fn managed(source: &str) -> cookie_agent_models::ModelsDevProvider {
    match toml::from_str::<ProviderDefinition>(source).unwrap() {
        ProviderDefinition::ModelsDev(provider) => provider,
        ProviderDefinition::Custom(_) => panic!("expected managed provider"),
    }
}

fn custom(source: &str) -> cookie_agent_models::CustomProvider {
    match toml::from_str::<ProviderDefinition>(source).unwrap() {
        ProviderDefinition::Custom(provider) => provider,
        ProviderDefinition::ModelsDev(_) => panic!("expected custom provider"),
    }
}

#[test]
fn removed_future_options_are_plain_unknown_fields() {
    let direct = serde_json::from_value::<cookie_agent_models::ProviderOptions>(
        serde_json::json!({"protocol_mode":"standard"}),
    )
    .unwrap_err();
    assert!(direct.to_string().contains("unknown field `protocol_mode`"));

    let definition = toml::from_str::<ProviderDefinition>(
        r#"source = "custom"
endpoint = "https://example.test/v1"
adaptor = "openai-responses"
auth = { method = "no-auth-v1", values = {} }
[models.test]
display_name = "Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = false, top_p = false, seed = false, native_replay = "unsupported", native_compaction = "unsupported", cancellation = "local_only", media = {} }
[models.test.options]
protocol_mode = "compact"
"#,
    )
    .unwrap_err();
    assert!(
        definition
            .to_string()
            .contains("unknown field `protocol_mode`")
    );
}

#[test]
fn invalid_managed_model_quarantines_locally_and_valid_slashed_sibling_survives() {
    let compiler = DynamicCompiler::registry1();
    let record = provider(
        "openrouter",
        "@openrouter/ai-sdk-provider",
        Some("https://openrouter.ai/api/v1"),
        &["OPENROUTER_API_KEY"],
        [model("org/good"), {
            let mut bad = model("org/bad");
            bad.provider = Some(CatalogModelProviderClaims {
                npm: Some("@ai-sdk/openai-compatible".to_owned()),
                api: None,
                shape: None,
            });
            bad
        }],
    );
    let authored = managed(
        r#"
source = "models_dev"
api_key = "secret"
"#,
    );
    let compiled = compiler
        .compile_managed("sha256:test", &record, Some(&authored))
        .unwrap();
    assert!(
        compiled
            .models
            .contains_key(&ProviderModelId::new("org/good").unwrap())
    );
    assert_eq!(
        compiled.quarantined_models,
        [cookie_agent_models::compiler::ModelQuarantine {
            id: ProviderModelId::new("org/bad").unwrap(),
            reason: RecipeQuarantineReason::CatalogModelProviderNpmDrift,
        }]
    );
}

#[test]
fn managed_defaults_include_supported_text_models_and_sparse_overrides_only() {
    let compiler = DynamicCompiler::registry1();
    let mut deprecated = model("gpt-4o-old");
    deprecated.status = CatalogModelStatus::Deprecated;
    let record = provider(
        "openai",
        "@ai-sdk/openai",
        None,
        &["OPENAI_API_KEY"],
        [model("gpt-5-mini"), model("gpt-4o"), deprecated],
    );
    let authored = managed(
        r#"
source = "models_dev"
api_key = "secret"

[model_overrides."gpt-4o"]
display_name = "Reviewed Chat"
defaults = { max_output_tokens = 1024 }
"#,
    );
    let compiled = compiler
        .compile_managed("sha256:test", &record, Some(&authored))
        .unwrap();
    assert_eq!(compiled.models.len(), 2);
    let responses = &compiled.models[&ProviderModelId::new("gpt-5-mini").unwrap()];
    assert_eq!(responses.adapter.id(), "openai-responses");
    assert_eq!(responses.status, CompiledModelStatus::Available);
    let chat = &compiled.models[&ProviderModelId::new("gpt-4o").unwrap()];
    assert_eq!(chat.adapter.id(), "openai-chat");
    assert_eq!(chat.display_name, "Reviewed Chat");
    assert_eq!(chat.defaults.max_output_tokens, Some(1024));
}

#[test]
fn managed_secret_rotation_does_not_change_fingerprints_but_auth_shape_does() {
    let compiler = DynamicCompiler::registry1();
    let record = provider(
        "groq",
        "@ai-sdk/groq",
        None,
        &["GROQ_API_KEY"],
        [model("llama/model")],
    );
    let one = managed("source = \"models_dev\"\napi_key = \"one\"\n");
    let two = managed("source = \"models_dev\"\napi_key = \"two\"\n");
    let absent = managed("source = \"models_dev\"\n");
    let first = compiler
        .compile_managed("sha256:test", &record, Some(&one))
        .unwrap();
    let rotated = compiler
        .compile_managed("sha256:test", &record, Some(&two))
        .unwrap();
    let unavailable = compiler
        .compile_managed("sha256:test", &record, Some(&absent))
        .unwrap();
    assert_eq!(first.fingerprint, rotated.fingerprint);
    assert_ne!(first.fingerprint, unavailable.fingerprint);
    assert_eq!(
        unavailable.models.values().next().unwrap().auth.source,
        AuthSourceCategory::Unavailable
    );
}

#[test]
fn custom_compiler_validates_auth_static_headers_capability_ceiling_and_fingerprints() {
    let source = |secret: &str, header: &str| {
        format!(
            r#"
source = "custom"
endpoint = "https://gateway.example/v1"
adaptor = "openai-compatible"
auth = {{ method = "bearer-api-key-v1", values = {{ api_key = "{secret}" }} }}
headers = {{ "x-routing" = "{header}" }}

[models."org/model"]
display_name = "Gateway Model"

[models."org/model".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 8192
output_tokens = 2048
tool_calling = true
parallel_tool_calls = false
structured_output = true
reasoning = false
temperature = true
top_p = false
seed = false
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {{}}
"#
        )
    };
    let compiler = DynamicCompiler::registry1();
    let id = ProviderId::new("custom.gateway").unwrap();
    let first = compiler
        .compile_custom(&id, &custom(&source("one", "a")))
        .unwrap();
    let rotated = compiler
        .compile_custom(&id, &custom(&source("two", "a")))
        .unwrap();
    let behavior_changed = compiler
        .compile_custom(&id, &custom(&source("one", "b")))
        .unwrap();
    assert_eq!(first.fingerprint, rotated.fingerprint);
    assert_ne!(first.fingerprint, behavior_changed.fingerprint);
    assert!(
        first
            .models
            .contains_key(&ProviderModelId::new("org/model").unwrap())
    );

    let collision = source("one", "a").replace(
        "headers = { \"x-routing\" = \"a\" }",
        "headers = { authorization = \"not-secret\" }",
    );
    assert!(matches!(
        compiler.compile_custom(&id, &custom(&collision)),
        Err(DynamicCompileError::StaticHeaders)
    ));
}

#[test]
fn vertex_requires_explicit_setup_and_access_token_and_rejects_non_gemini_families() {
    let compiler = DynamicCompiler::registry1();
    let mut gemini = model("gemini-2.5-pro");
    gemini.family = Some("gemini-pro".to_owned());
    let mut gpt_oss = model("openai/gpt-oss-20b-maas");
    gpt_oss.family = Some("gpt-oss".to_owned());
    let record = provider(
        "google-vertex",
        "@ai-sdk/google-vertex",
        None,
        &[
            "GOOGLE_VERTEX_PROJECT",
            "GOOGLE_VERTEX_LOCATION",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ],
        [gemini, gpt_oss],
    );
    let authored = managed(
        r#"
source = "models_dev"
setup = { project = "test-project", location = "us-central1" }
auth_override = { method = "oauth-access-token-v1", values = { access_token = "token" } }
"#,
    );
    let compiled = compiler
        .compile_managed("sha256:test", &record, Some(&authored))
        .unwrap();
    let gemini = &compiled.models[&ProviderModelId::new("gemini-2.5-pro").unwrap()];
    assert_eq!(gemini.status, CompiledModelStatus::Available);
    assert_eq!(
        gemini.endpoint.as_deref(),
        Some(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/test-project/locations/us-central1/publishers/google"
        )
    );
    assert_eq!(
        compiled.quarantined_models[0].reason,
        RecipeQuarantineReason::UnsupportedVertexModelFamily
    );
}

#[test]
fn authored_cloud_base_urls_and_ambient_future_auth_are_not_accepted() {
    let provider = custom(
        r#"
source = "custom"
endpoint = "https://example.test/v1"
adaptor = "aws-bedrock-converse"
setup = { region = "us-east-1" }
auth = { method = "aws-sdk-v1", values = {} }
[models.m]
display_name = "M"
[models.m.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 1000
output_tokens = 100
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = false
top_p = false
seed = false
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}
"#,
    );
    assert!(matches!(
        DynamicCompiler::registry1()
            .compile_custom(&ProviderId::new("custom.bedrock").unwrap(), &provider,),
        Err(DynamicCompileError::Auth)
    ));
}

#[test]
fn unknown_sparse_override_is_a_provider_configuration_error() {
    let compiler = DynamicCompiler::registry1();
    let record = provider(
        "anthropic",
        "@ai-sdk/anthropic",
        None,
        &["ANTHROPIC_API_KEY"],
        [model("claude-test")],
    );
    let authored = managed(
        r#"
source = "models_dev"
api_key = "secret"
[model_overrides.absent]
enabled = false
"#,
    );
    assert!(matches!(
        compiler.compile_managed("sha256:test", &record, Some(&authored)),
        Err(DynamicCompileError::UnknownModelOverride)
    ));
}

#[test]
fn custom_models_are_atomic() {
    let source = r#"
source = "custom"
endpoint = "https://gateway.example/v1"
adaptor = "cohere-v2-chat"
auth = { method = "bearer-api-key-v1", values = { api_key = "secret" } }

[models.good]
display_name = "Good"
[models.good.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 1000
output_tokens = 100
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = false
top_p = false
seed = false
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}

[models.bad]
display_name = "Bad"
[models.bad.capabilities]
input = ["text", "audio"]
output = ["text"]
context_tokens = 1000
output_tokens = 100
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = false
top_p = false
seed = false
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
[models.bad.capabilities.media.audio]
mime_types = ["audio/wav"]
max_bytes = 1000
max_count = 1
"#;
    assert!(matches!(
        DynamicCompiler::registry1()
            .compile_custom(&ProviderId::new("custom.atomic").unwrap(), &custom(source),),
        Err(DynamicCompileError::CustomModel)
    ));
}

#[test]
fn provider_level_claim_drift_removes_all_children() {
    let record = provider(
        "deepinfra",
        "@ai-sdk/openai-compatible",
        None,
        &["DEEPINFRA_API_KEY"],
        [model("valid/model")],
    );
    let compiled = DynamicCompiler::registry1()
        .compile_managed("sha256:test", &record, None)
        .unwrap();
    assert!(compiled.models.is_empty());
    assert_eq!(
        compiled.provider_quarantine,
        Some(RecipeQuarantineReason::CatalogProviderNpmDrift)
    );
}

#[test]
fn presence_complete_provider_and_model_shape_drift_stays_typed() {
    let compiler = DynamicCompiler::registry1();
    let mut provider_shape = provider(
        "groq",
        "@ai-sdk/groq",
        None,
        &["GROQ_API_KEY"],
        [model("valid/model")],
    );
    provider_shape.shape = Some("responses".to_owned());
    provider_shape.claims.shape = CatalogClaim::Present("responses".to_owned());
    assert_eq!(
        compiler
            .compile_managed("sha256:test", &provider_shape, None)
            .unwrap()
            .provider_quarantine,
        Some(RecipeQuarantineReason::CatalogProviderShapeDrift)
    );

    let mut shaped_model = model("valid/model");
    shaped_model.shape = Some("responses".to_owned());
    let record = provider(
        "groq",
        "@ai-sdk/groq",
        None,
        &["GROQ_API_KEY"],
        [shaped_model],
    );
    assert_eq!(
        compiler
            .compile_managed("sha256:test", &record, None)
            .unwrap()
            .quarantined_models[0]
            .reason,
        RecipeQuarantineReason::CatalogModelShapeDrift
    );
}

#[test]
fn selected_endpoint_policy_rejects_loopback_managed_overrides_and_unreviewed_custom_http() {
    let compiler = DynamicCompiler::registry1();
    let record = provider(
        "groq",
        "@ai-sdk/groq",
        None,
        &["GROQ_API_KEY"],
        [model("valid/model")],
    );
    let managed = managed(
        "source = \"models_dev\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key = \"must-not-attach\"\n",
    );
    assert!(matches!(
        compiler.compile_managed("sha256:test", &record, Some(&managed)),
        Err(DynamicCompileError::Endpoint)
    ));

    let custom_source = |endpoint: &str| {
        format!(
            r#"source = "custom"
endpoint = "{endpoint}"
adaptor = "openai-compatible"
auth = {{ method = "bearer-api-key-v1", values = {{ api_key = "must-not-attach" }} }}
[models.test]
display_name = "Test"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = false, top_p = false, seed = false, native_replay = "unsupported", native_compaction = "unsupported", cancellation = "local_only", media = {{}} }}
"#
        )
    };
    for endpoint in [
        "http://127.0.0.2:9/v1",
        "http://127.0.0.1:9/other",
        "http://127.0.0.1/v1",
    ] {
        assert!(matches!(
            compiler.compile_custom(
                &ProviderId::new("custom.exfiltration").unwrap(),
                &custom(&custom_source(endpoint)),
            ),
            Err(DynamicCompileError::Endpoint)
        ));
    }
    assert!(
        compiler
            .compile_custom(
                &ProviderId::new("custom.reviewed-loopback").unwrap(),
                &custom(&custom_source("http://127.0.0.1:9/v1")),
            )
            .is_ok()
    );
}

#[test]
fn cohere_exact_compatibility_exception_routes_only_the_named_model() {
    let compiler = DynamicCompiler::registry1();
    let mut north = model("north-mini-code-1-0");
    north.provider = Some(CatalogModelProviderClaims {
        npm: Some("@ai-sdk/openai-compatible".to_owned()),
        api: Some("https://api.cohere.ai/compatibility/v1".to_owned()),
        shape: None,
    });
    let record = provider(
        "cohere",
        "@ai-sdk/cohere",
        None,
        &["COHERE_API_KEY"],
        [north],
    );
    let authored = managed("source = \"models_dev\"\napi_key = \"secret\"\n");
    let compiled = compiler
        .compile_managed("sha256:test", &record, Some(&authored))
        .unwrap();
    let model = &compiled.models[&ProviderModelId::new("north-mini-code-1-0").unwrap()];
    assert_eq!(model.adapter.id(), "openai-compatible");
    assert_eq!(
        model.endpoint.as_deref(),
        Some("https://api.cohere.ai/compatibility/v1")
    );
}

#[test]
fn exact_vertex_bedrock_and_azure_overrides_are_typed_unsupported_not_inferred() {
    let compiler = DynamicCompiler::registry1();

    let mut vertex_override = model("claude-on-vertex");
    vertex_override.provider = Some(CatalogModelProviderClaims {
        npm: Some("@ai-sdk/google-vertex/anthropic".to_owned()),
        api: None,
        shape: None,
    });
    let vertex = provider(
        "google-vertex",
        "@ai-sdk/google-vertex",
        None,
        &[
            "GOOGLE_VERTEX_PROJECT",
            "GOOGLE_VERTEX_LOCATION",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ],
        [vertex_override],
    );
    assert_eq!(
        compiler
            .compile_managed("sha256:test", &vertex, None)
            .unwrap()
            .quarantined_models[0]
            .reason,
        RecipeQuarantineReason::UnsupportedProtocolFeature
    );

    let mut mantle = model("openai.gpt-oss");
    mantle.provider = Some(CatalogModelProviderClaims {
        npm: Some("@ai-sdk/amazon-bedrock/mantle".to_owned()),
        api: Some("https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1".to_owned()),
        shape: Some("responses".to_owned()),
    });
    let bedrock = provider(
        "amazon-bedrock",
        "@ai-sdk/amazon-bedrock",
        None,
        &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_REGION",
            "AWS_BEARER_TOKEN_BEDROCK",
        ],
        [mantle],
    );
    assert_eq!(
        compiler
            .compile_managed("sha256:test", &bedrock, None)
            .unwrap()
            .quarantined_models[0]
            .reason,
        RecipeQuarantineReason::UnsupportedProtocolFeature
    );

    let mut azure_override = model("claude-on-azure");
    azure_override.provider = Some(CatalogModelProviderClaims {
        npm: Some("@ai-sdk/anthropic".to_owned()),
        api: Some("https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1".to_owned()),
        shape: None,
    });
    let azure = provider(
        "azure",
        "@ai-sdk/azure",
        None,
        &["AZURE_RESOURCE_NAME", "AZURE_API_KEY"],
        [azure_override],
    );
    assert_eq!(
        compiler
            .compile_managed("sha256:test", &azure, None)
            .unwrap()
            .quarantined_models[0]
            .reason,
        RecipeQuarantineReason::UnsupportedProtocolFeature
    );
}
