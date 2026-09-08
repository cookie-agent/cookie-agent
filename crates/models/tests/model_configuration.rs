use cookie_agent_identity::{ProviderId, ProviderModelId, VariantId};
use cookie_agent_models::{
    ProviderDefinition, ReplayCapability, RequestEndpoint,
    adapters::OvenAdapterFamily,
    compiler::{CompiledDynamicProvider, DynamicCompiler},
};

fn custom(extra: &str) -> String {
    format!(
        r#"
source = "custom"
endpoint = "https://example.invalid/v1"
adaptor = "openai-compatible"
auth = {{ method = "no-auth-v1", values = {{}} }}
[models."qwen3.8-flash"]
display_name = "Qwen"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 32768, output_tokens = 4096, reasoning = true, temperature = true, top_p = true, seed = false, media = {{}} }}
{extra}
"#
    )
}

fn compile(text: &str) -> Result<CompiledDynamicProvider, String> {
    let definition: ProviderDefinition = toml::from_str(text).map_err(|error| error.to_string())?;
    let id = ProviderId::new("cookie-api").unwrap();
    definition
        .validate_for(&id)
        .map_err(|error| error.to_string())?;
    let ProviderDefinition::Custom(provider) = definition else {
        panic!("custom provider")
    };
    DynamicCompiler::default()
        .compile_custom(&id, &provider)
        .map_err(|error| error.to_string())
}

#[test]
fn qwen_reasoning_only_variants_inherit_base_and_emit_each_effort() {
    let compiled = compile(&custom(
        r#"
generation_options = { temperature = 1.0, top_p = 0.95, max_output_tokens = 1234 }
adaptor_options = { request_endpoint = "completions" }
[models."qwen3.8-flash".variants.low]
reasoning = { type = "effort", value = "low" }
[models."qwen3.8-flash".variants.medium]
reasoning = { type = "effort", value = "medium" }
[models."qwen3.8-flash".variants.xhigh]
reasoning = { type = "effort", value = "xhigh" }
"#,
    ))
    .unwrap();
    let model = &compiled.models[&ProviderModelId::new("qwen3.8-flash").unwrap()];
    assert!(model.capabilities.tool_calling);
    assert!(model.capabilities.parallel_tool_calls);
    assert!(model.capabilities.structured_output);
    assert_eq!(model.capabilities.native_replay, ReplayCapability::Optional);
    assert_eq!(
        model.capabilities.cancellation,
        cookie_agent_models::CancellationCapability::LocalOnly
    );
    for effort in ["low", "medium", "xhigh"] {
        let variant = &model.variants[&VariantId::new(effort).unwrap()];
        assert_eq!(variant.defaults, model.defaults);
        assert_eq!(
            variant.options.request_endpoint,
            Some(RequestEndpoint::Completions)
        );
        let request = cookie_agent_models::ResolvedRequestDefaults {
            request: variant.defaults.clone(),
            reasoning: variant.reasoning.clone(),
        }
        .apply(&variant.options, oven_sdk::Request::new(vec![]));
        assert_eq!(request.inference.reasoning_effort.as_deref(), Some(effort));
        assert_eq!(request.inference.temperature, Some(1.0));
        assert_eq!(request.inference.max_output_tokens, Some(1234));
    }
}

#[test]
fn sparse_lists_and_false_values_override_without_masking_other_base_fields() {
    let text = custom(
        r#"
generation_options = { temperature = 0.5, stop = ["END"] }
adaptor_options = { beta = ["a"] }
[models."qwen3.8-flash".variants.clear]
generation_options = { stop = [] }
adaptor_options = { beta = [] }
reasoning = { type = "toggle", enabled = false }
"#,
    )
    .replace("openai-compatible", "anthropic-compatible")
    .replace(
        "method = \"no-auth-v1\", values = {}",
        "method = \"anthropic-api-key-v1\", values = { api_key = \"test\" }",
    );
    let compiled = compile(&text).unwrap();
    let model = compiled.models.values().next().unwrap();
    let variant = model.variants.values().next().unwrap();
    assert!(variant.defaults.stop.is_empty());
    assert!(variant.options.beta.is_empty());
    assert_eq!(variant.defaults.temperature, model.defaults.temperature);
    assert_eq!(
        variant.reasoning,
        Some(cookie_agent_models::ReasoningBehavior::Toggle { enabled: false })
    );
}

#[test]
fn disabling_is_a_noop_for_absent_ids_but_rejects_settings_and_named_defaults() {
    assert!(
        compile(&custom("variants = { absent = { enabled = false } }"))
            .unwrap()
            .models
            .values()
            .next()
            .unwrap()
            .variants
            .is_empty()
    );
    for extra in [
        "variants = { off = { enabled = false, generation_options = {} } }",
        "variants = { off = { enabled = false, adaptor_options = {} } }",
        "variants = { off = { enabled = false, headers = {} } }",
        "variants = { off = { enabled = false, display_name = \"Off\" } }",
        "default_variant = \"off\"\nvariants = { off = { enabled = false } }",
    ] {
        assert!(compile(&custom(extra)).is_err(), "{extra}");
    }
}

#[test]
fn removed_and_unknown_model_fields_are_strictly_rejected() {
    for extra in [
        "defaults = {}",
        "options = {}",
        "shape = \"chat\"",
        "unknown = true",
        "adaptor_options = { api_path = \"/chat/completions\" }",
        "adaptor_options = { request_endpoint = \"chat\" }",
        "adaptor_options = { arbitrary = false }",
        "variants = { low = { operation = \"add\" } }",
        "variants = { low = { defaults = {} } }",
        "variants = { low = { options = {} } }",
        "variants = { low = { pricing = {} } }",
        "variants = { low = { reasoning = { type = \"effort\", value = \"low\", enabled = false } } }",
    ] {
        assert!(
            toml::from_str::<ProviderDefinition>(&custom(extra)).is_err(),
            "{extra}"
        );
    }
    assert!(
        compile(&custom("").replace("media = {}", "cancellation = \"local_only\", media = {}"))
            .is_err()
    );
    for text in [
        "source = \"models_dev\"\nmodel_overrides = {}",
        "source = \"models_dev\"\nshape = \"responses\"",
        "source = \"models_dev\"\nmodels = { x = { capabilities = {} } }",
    ] {
        assert!(
            toml::from_str::<ProviderDefinition>(text).is_err(),
            "{text}"
        );
    }
    assert!(compile(&custom("").replace("display_name = \"Qwen\"", "")).is_err());
}

#[test]
fn explicit_capabilities_and_replay_are_authoritative() {
    let text = custom("").replace("reasoning = true", "reasoning = false, tool_calling = false, parallel_tool_calls = false, structured_output = false, native_replay = \"unsupported\"");
    let compiled = compile(&text).unwrap();
    let capabilities = &compiled.models.values().next().unwrap().capabilities;
    assert!(
        !capabilities.tool_calling
            && !capabilities.parallel_tool_calls
            && !capabilities.structured_output
    );
    assert_eq!(capabilities.native_replay, ReplayCapability::Unsupported);
    assert!(
        compile(&custom("").replace("reasoning = true", "reasoning = true, tool_calling = false"))
            .unwrap_err()
            .contains("parallel_tool_calls = false")
    );
    assert!(
        compile(
            &custom("adaptor_options = { request_endpoint = \"responses\" }").replace(
                "reasoning = true",
                "reasoning = true, native_replay = \"unsupported\""
            )
        )
        .is_err()
    );
}

#[test]
fn endpoint_matrix_and_inherited_endpoint_conflicts_are_validated() {
    assert_eq!(
        compile(&custom(
            "adaptor_options = { request_endpoint = \"responses\" }"
        ))
        .unwrap()
        .models
        .values()
        .next()
        .unwrap()
        .adapter,
        OvenAdapterFamily::OpenaiResponses
    );
    assert!(compile(&custom("adaptor_options = { request_endpoint = \"responses\", store = false }\nvariants = { chat = { adaptor_options = { request_endpoint = \"completions\" } } }")).is_err());
    assert!(
        compile(&custom(
            "adaptor_options = { request_endpoint = \"responses\", store = true }"
        ))
        .unwrap_err()
        .contains("store = true")
    );
    let anthropic = custom("adaptor_options = { request_endpoint = \"responses\" }")
        .replace("openai-compatible", "anthropic-compatible")
        .replace(
            "method = \"no-auth-v1\", values = {}",
            "method = \"anthropic-api-key-v1\", values = { api_key = \"test\" }",
        );
    assert!(
        compile(&anthropic)
            .unwrap_err()
            .contains("request_endpoint")
    );
}

#[test]
fn wire_model_id_is_sparse_strict_and_independent_of_local_identity() {
    let compiled = compile(&custom(
        r#"
model_id = "vendor/backend[fast]:v1"
pricing = { input_per_million_usd = "0.125" }
variants = { inherited = {}, other = { model_id = "vendor/backend:v2" } }
"#,
    ))
    .unwrap();
    let model = &compiled.models[&ProviderModelId::new("qwen3.8-flash").unwrap()];
    assert_eq!(model.id.as_str(), "qwen3.8-flash");
    assert_eq!(model.wire_model_id.as_str(), "vendor/backend[fast]:v1");
    assert!(
        model.variants[&VariantId::new("inherited").unwrap()]
            .model_id
            .is_none()
    );
    assert_eq!(
        model.variants[&VariantId::new("other").unwrap()]
            .model_id
            .as_ref()
            .unwrap()
            .as_str(),
        "vendor/backend:v2"
    );
    for bad in [
        "\"\"",
        "\" \"",
        "\" leading\"",
        "\"trailing \"",
        "\"a\\nb\"",
        "1",
        "true",
        "[]",
        "{}",
    ] {
        for extra in [
            format!("model_id = {bad}"),
            format!("variants = {{ invalid = {{ model_id = {bad} }} }}"),
        ] {
            assert!(compile(&custom(&extra)).is_err(), "{extra}");
        }
    }
    assert!(
        compile(&custom(
            "variants = { disabled = { enabled = false, model_id = \"wire\" } }"
        ))
        .is_err()
    );
    assert!(compile(&custom(&format!("model_id = \"{}\"", "x".repeat(2049)))).is_err());
}
