use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    net::TcpListener,
    sync::{Arc, mpsc},
    thread,
};

use cookie_agent_identity::{ModelKey, ModelSelection, ProviderId};
use cookie_agent_models::{Catalog, ProviderDefinition, build_model_set};
use oven_sdk::{AbortSignal, HistoryTurn, InputPart, Request, TextPart, UserMessage};

fn explicit_provider() -> ProviderDefinition {
    toml::from_str(
        r#"
source = "explicit"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"
auth = { type = "none" }

[models."arbitrary-model"]
display_name = "Arbitrary Model"
default_variant = "fast"

[models."arbitrary-model".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 8192
output_tokens = 2048
tool_calling = true
parallel_tool_calls = true
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}

[models."arbitrary-model".variants.fast]
operation = "add"
defaults = { temperature = 0.1, seed = 7 }
"#,
    )
    .unwrap()
}

#[test]
fn direct_keys_variants_and_frozen_bindings_are_exact() {
    let providers = BTreeMap::from([(ProviderId::new("gateway").unwrap(), explicit_provider())]);
    let set = build_model_set(&providers, &Catalog::embedded().unwrap(), None).unwrap();
    let key: ModelKey = "gateway/arbitrary-model".parse().unwrap();
    let entry = set.get(&key).unwrap();
    assert_eq!(entry.default_variant().unwrap().as_str(), "fast");
    assert!(
        entry
            .variants()
            .contains_key(entry.default_variant().unwrap())
    );
    let selection = ModelSelection {
        model: key,
        variant: entry.default_variant().cloned(),
    };
    let binding = set.freeze(&selection).unwrap();
    assert_eq!(
        &set.resolve(&binding).unwrap().selection().model,
        &selection.model
    );
    assert_eq!(binding.resolved.selection, selection);
}

#[test]
fn public_descriptors_are_available_enabled_models_in_model_key_order() {
    let capabilities = || {
        serde_json::json!({
            "input": ["text"],
            "output": ["text"],
            "context_tokens": 8192,
            "output_tokens": 2048,
            "tool_calling": true,
            "parallel_tool_calls": false,
            "structured_output": false,
            "reasoning": false,
            "temperature": true,
            "top_p": true,
            "seed": true,
            "native_replay": "unsupported",
            "native_compaction": "unsupported",
            "cancellation": "local_only",
            "media": {}
        })
    };
    let provider = |models: serde_json::Value| {
        serde_json::from_value::<ProviderDefinition>(serde_json::json!({
            "source": "explicit",
            "endpoint": "https://example.test/v1",
            "adaptor": "openai-compatible",
            "auth": {"type": "none"},
            "models": models
        }))
        .unwrap()
    };
    let providers = BTreeMap::from([
        (
            ProviderId::new("z-provider").unwrap(),
            provider(serde_json::json!({
                "alpha": {"display_name": "Alpha", "capabilities": capabilities()}
            })),
        ),
        (
            ProviderId::new("a-provider").unwrap(),
            provider(serde_json::json!({
                "zeta": {"display_name": "Zeta", "capabilities": capabilities()},
                "alpha": {"display_name": "Alpha", "capabilities": capabilities()},
                "disabled": {
                    "enabled": false,
                    "display_name": "Disabled",
                    "capabilities": capabilities()
                }
            })),
        ),
    ]);
    let set = build_model_set(&providers, &Catalog::embedded().unwrap(), None).unwrap();

    assert_eq!(
        set.descriptors()
            .into_iter()
            .map(|descriptor| descriptor.key.to_string())
            .collect::<Vec<_>>(),
        ["a-provider/alpha", "a-provider/zeta", "z-provider/alpha"]
    );
}

#[test]
fn secret_values_do_not_change_behavior_fingerprints() {
    let source = |secret: &str| {
        toml::from_str::<ProviderDefinition>(&format!(
            r#"
source = "explicit"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"
auth = {{ type = "bearer", token = "{secret}" }}
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
media = {{}}
"#
        ))
        .unwrap()
    };
    let catalog = Catalog::embedded().unwrap();
    let first = build_model_set(
        &BTreeMap::from([(ProviderId::new("p").unwrap(), source("one"))]),
        &catalog,
        None,
    )
    .unwrap();
    let second = build_model_set(
        &BTreeMap::from([(ProviderId::new("p").unwrap(), source("two"))]),
        &catalog,
        None,
    )
    .unwrap();
    assert_eq!(first.fingerprint(), second.fingerprint());
}

#[test]
fn header_names_but_not_header_values_change_behavior_fingerprints() {
    let source = |header: &str, value: &str| {
        toml::from_str::<ProviderDefinition>(&format!(
            r#"
source = "explicit"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"
auth = {{ type = "none" }}
headers = {{ "{header}" = "{value}" }}
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
media = {{}}
"#
        ))
        .unwrap()
    };
    let build = |definition| {
        build_model_set(
            &BTreeMap::from([(ProviderId::new("p").unwrap(), definition)]),
            &Catalog::embedded().unwrap(),
            None,
        )
        .unwrap()
    };
    let first = build(source("x-structure-one", "secret-one"));
    let rotated = build(source("x-structure-one", "secret-two"));
    let renamed = build(source("x-structure-two", "secret-one"));
    assert_eq!(first.fingerprint(), rotated.fingerprint());
    assert_ne!(first.fingerprint(), renamed.fingerprint());
}

#[test]
fn every_reviewed_explicit_adaptor_constructs_from_its_declared_auth_and_options() {
    let cases = [
        (
            "anthropic",
            "auth = { type = \"api_key\", key = \"x\" }",
            "",
        ),
        (
            "openai-chat",
            "auth = { type = \"bearer\", token = \"x\" }",
            "",
        ),
        (
            "openai-responses",
            "auth = { type = \"bearer\", token = \"x\" }",
            "",
        ),
        ("openai-compatible", "auth = { type = \"none\" }", ""),
        (
            "google-gemini",
            "auth = { type = \"api_key\", key = \"x\" }",
            "",
        ),
        (
            "google-vertex-gemini",
            "auth = { type = \"fields\", values = { access_token = \"x\" } }",
            "[models.m.options]\nproject = \"project\"\nlocation = \"us-central1\"",
        ),
        (
            "aws-bedrock-converse",
            "auth = { type = \"fields\", values = { access_key_id = \"x\", secret_access_key = \"y\" } }",
            "[models.m.options]\nregion = \"us-east-1\"",
        ),
        (
            "azure-openai-chat",
            "auth = { type = \"api_key\", key = \"x\" }",
            "[models.m.options]\ndeployment = \"deployment\"\napi_version = \"2026-01-01\"",
        ),
        (
            "azure-openai-responses",
            "auth = { type = \"api_key\", key = \"x\" }",
            "[models.m.options]\ndeployment = \"deployment\"\napi_version = \"2026-01-01\"",
        ),
        (
            "cohere-v2-chat",
            "auth = { type = \"bearer\", token = \"x\" }",
            "",
        ),
        (
            "open-responses",
            "auth = { type = \"bearer\", token = \"x\" }",
            "",
        ),
    ];
    let catalog = Catalog::embedded().unwrap();
    for (index, (adaptor, auth, options)) in cases.into_iter().enumerate() {
        let endpoint = match adaptor {
            "azure-openai-chat" | "azure-openai-responses" => "https://example.test",
            "cohere-v2-chat" => "https://example.test/v2/chat",
            "open-responses" => "https://example.test/v1/responses",
            _ => "https://example.test/v1",
        };
        let source = format!(
            r#"
source = "explicit"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
{auth}
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
media = {{}}
{options}
"#
        );
        let provider: ProviderDefinition =
            toml::from_str(&source).unwrap_or_else(|error| panic!("{adaptor} parse: {error}"));
        let id = ProviderId::new(format!("p{index}")).unwrap();
        build_model_set(&BTreeMap::from([(id, provider)]), &catalog, None)
            .unwrap_or_else(|error| panic!("{adaptor} build: {error}"));
    }
}

#[test]
fn structural_variants_build_distinct_exact_executables_and_bindings() {
    let cases = [
        (
            "openai-compatible",
            "https://example.test/ignored",
            "auth = { type = \"none\" }",
            "api_path = \"/base/chat/completions\"",
            "api_path = \"/variant/chat/completions\"",
        ),
        (
            "openai-chat",
            "https://example.test/v1",
            "auth = { type = \"bearer\", token = \"x\" }",
            "organization = \"base-org\"\nproject = \"base-project\"",
            "organization = \"variant-org\"\nproject = \"variant-project\"",
        ),
        (
            "google-gemini",
            "https://example.test/v1beta",
            "auth = { type = \"api_key\", key = \"x\" }",
            "api_version = \"v1beta\"",
            "api_version = \"v1\"",
        ),
        (
            "google-vertex-gemini",
            "https://example.test/v1",
            "auth = { type = \"fields\", values = { access_token = \"x\" } }",
            "project = \"base-project\"\nlocation = \"us-central1\"",
            "project = \"variant-project\"\nlocation = \"europe-west1\"",
        ),
        (
            "aws-bedrock-converse",
            "https://example.test/v1",
            "auth = { type = \"fields\", values = { access_key_id = \"x\", secret_access_key = \"y\" } }",
            "region = \"us-east-1\"",
            "region = \"us-west-2\"",
        ),
        (
            "azure-openai-chat",
            "https://example.test",
            "auth = { type = \"api_key\", key = \"x\" }",
            "deployment = \"base\"\napi_version = \"2026-01-01\"",
            "deployment = \"variant\"\napi_version = \"2026-02-02\"",
        ),
        (
            "azure-openai-responses",
            "https://example.test",
            "auth = { type = \"api_key\", key = \"x\" }",
            "deployment = \"base\"\napi_version = \"2026-01-01\"",
            "deployment = \"variant\"\napi_version = \"2026-02-02\"",
        ),
        (
            "open-responses",
            "https://example.test/v1/responses",
            "auth = { type = \"bearer\", token = \"x\" }",
            "protocol_mode = \"standard\"",
            "protocol_mode = \"compact\"",
        ),
    ];
    for (index, (adaptor, endpoint, auth, base_options, variant_options)) in
        cases.into_iter().enumerate()
    {
        let definition: ProviderDefinition = toml::from_str(&format!(
            r#"
source = "explicit"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
{auth}
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
media = {{}}
[models.m.options]
{base_options}
[models.m.variants.structural]
operation = "add"
[models.m.variants.structural.options]
{variant_options}
"#
        ))
        .unwrap_or_else(|error| panic!("{adaptor} parse: {error}"));
        let provider_id = ProviderId::new(format!("structural-{index}")).unwrap();
        let set = build_model_set(
            &BTreeMap::from([(provider_id.clone(), definition)]),
            &Catalog::embedded().unwrap(),
            None,
        )
        .unwrap_or_else(|error| panic!("{adaptor} build: {error}"));
        let key = ModelKey::new(provider_id, "m".parse().unwrap()).unwrap();
        let base = set
            .resolve_selection(&ModelSelection {
                model: key.clone(),
                variant: None,
            })
            .unwrap();
        let variant = set
            .resolve_selection(&ModelSelection {
                model: key,
                variant: Some("structural".parse().unwrap()),
            })
            .unwrap();
        assert!(!Arc::ptr_eq(base.model(), variant.model()), "{adaptor}");
        assert_ne!(
            base.behavior_fingerprint(),
            variant.behavior_fingerprint(),
            "{adaptor}"
        );
        let base_binding = set.freeze(base.selection()).unwrap();
        let variant_binding = set.freeze(variant.selection()).unwrap();
        assert_ne!(
            base_binding.resolved.selection_fingerprint,
            variant_binding.resolved.selection_fingerprint,
            "{adaptor}"
        );
        assert_eq!(
            set.resolve(&base_binding).unwrap().selection(),
            base.selection()
        );
        assert_eq!(
            set.resolve(&variant_binding).unwrap().selection(),
            variant.selection()
        );
    }
}

#[tokio::test]
async fn compatible_structural_variant_dispatches_to_its_exact_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let server = thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            sender
                .send(request.lines().next().unwrap().to_owned())
                .unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: [DONE]\n\n",
                )
                .unwrap();
        }
    });
    let definition: ProviderDefinition = toml::from_str(&format!(
        r#"
source = "explicit"
endpoint = "http://{address}/base"
adaptor = "openai-compatible"
auth = {{ type = "none" }}
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
media = {{}}
[models.m.variants.structural]
operation = "add"
[models.m.variants.structural.options]
api_path = "/variant/chat/completions"
"#
    ))
    .unwrap();
    let provider_id = ProviderId::new("captured").unwrap();
    let set = build_model_set(
        &BTreeMap::from([(provider_id.clone(), definition)]),
        &Catalog::embedded().unwrap(),
        None,
    )
    .unwrap();
    let key = ModelKey::new(provider_id, "m".parse().unwrap()).unwrap();
    for variant in [None, Some("structural".parse().unwrap())] {
        let resolved = set
            .resolve_selection(&ModelSelection {
                model: key.clone(),
                variant,
            })
            .unwrap();
        let request = Request::new(vec![HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new("hello")),
        ]))]);
        resolved
            .model()
            .stream(resolved.prepare_request(request), AbortSignal::default())
            .await
            .unwrap();
    }
    server.join().unwrap();
    assert_eq!(
        receiver.recv().unwrap(),
        "POST /base/chat/completions? HTTP/1.1"
    );
    assert_eq!(
        receiver.recv().unwrap(),
        "POST /variant/chat/completions? HTTP/1.1"
    );
}

#[test]
fn unsupported_structural_variant_rejects_the_whole_provider_candidate() {
    let definition: ProviderDefinition = toml::from_str(
        r#"
source = "explicit"
endpoint = "https://example.test/base"
adaptor = "openai-compatible"
auth = { type = "none" }
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
[models.good.variants.invalid]
operation = "add"
[models.good.variants.invalid.options]
api_path = "/not-encodable"
"#,
    )
    .unwrap();
    assert!(
        build_model_set(
            &BTreeMap::from([(ProviderId::new("atomic").unwrap(), definition)]),
            &Catalog::embedded().unwrap(),
            None,
        )
        .is_err()
    );
}
