use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock},
};

use cookie_agent_config::{ConfigError, PolicySnapshot, load, load_layered, simple_wildcard_match};
use insta::assert_json_snapshot;
use tempfile::TempDir;

const CAPABILITIES: &str = r#"
[models.MODEL.capabilities]
features = []
cancellation = "local_only"
compaction = "unsupported"

[models.MODEL.capabilities.limits]

[models.MODEL.capabilities.modalities]
input = ["text"]
output = ["text"]

[models.MODEL.capabilities.media]
input = {}

[models.MODEL.capabilities.replay]
policy = "never"
capability = "unsupported"
reasoning = false
"#;

fn model(alias: &str, header: &str, tail: &str) -> String {
    format!(
        "[models.{alias}]\n{header}\n{}\n{tail}\n",
        CAPABILITIES.replace("MODEL", alias)
    )
}

fn all_models() -> String {
    let mut config = String::new();
    config.push_str(&model(
        "anthropic",
        r#"provider_id = "anthropic"
model_id = "arbitrary-anthropic-id"
endpoint = "https://example.test/v1"
adaptor = "anthropic""#,
        r#"[models.anthropic.auth]
type = "api_key"
value = "anthropic-secret"
[models.anthropic.settings]
thinking = "none"
thinking_default_active = false
thinking_disable_allowed = false
effort = false
assistant_prefill = false
reject_non_default_sampling = false"#,
    ));
    config.push_str(&model(
        "openai_chat",
        r#"provider_id = "openai"
model_id = "arbitrary-chat-id"
endpoint = "https://example.test/v1"
adaptor = "openai-chat""#,
        r#"[models.openai_chat.auth]
type = "openai"
api_key = "openai-secret"
[models.openai_chat.settings]
system_message_role = "system"
max_tokens_field = "max_tokens"
stream_usage = false
structured_output = "unsupported"
reasoning_field = "none""#,
    ));
    config.push_str(&model(
        "openai_responses",
        r#"provider_id = "openai"
model_id = "arbitrary-responses-id"
endpoint = "https://example.test/v1"
adaptor = "openai-responses""#,
        r#"[models.openai_responses.auth]
type = "openai"
api_key = "openai-secret"
[models.openai_responses.settings]"#,
    ));
    config.push_str(&model(
        "compatible",
        r#"provider_id = "local-gateway"
model_id = "arbitrary-compatible-id"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible""#,
        r#"[models.compatible.auth]
type = "bearer"
token = "gateway-secret"
[models.compatible.headers]
x-routing = "route-one"
[models.compatible.defaults]
max_output_tokens = 4096
temperature = 0.25
top_p = 0.8
reasoning_effort = "medium"
include_raw = true
[models.compatible.settings]
adapter_id = "cookie.gateway.chat"
system_message_role = "developer"
max_tokens_field = "max_completion_tokens"
stream_usage = false
structured_output = "unsupported"
reasoning_field = "none"
[models.compatible.options.extra_body]
custom_flag = true"#,
    ));
    config.push_str(&model(
        "google",
        r#"provider_id = "google"
model_id = "arbitrary-google-id"
endpoint = "https://example.test/v1beta"
adaptor = "google""#,
        r#"[models.google.auth]
type = "api_key"
value = "google-secret"
[models.google.settings]
model_resource = "models/arbitrary-resource"
strict_functions = false
mixed_client_and_provider_tools = false
current_turn_signature_sentinel = false
[models.google.settings.thinking]
type = "unsupported""#,
    ));
    config.push_str(&model(
        "vertex",
        r#"provider_id = "google.vertex"
model_id = "arbitrary-vertex-id"
endpoint = "https://example.test/v1"
adaptor = "vertex""#,
        r#"[models.vertex.auth]
type = "access_token"
token = "vertex-secret"
[models.vertex.settings]
project = "project"
location = "us-central1"
thinking = "unsupported"
provider_tools = false
mixed_client_and_provider_tools = false
strict_functions = false
stream_function_call_arguments = false
[models.vertex.settings.resource]
type = "publisher_model"
publisher = "google"
model = "arbitrary-resource"
[models.vertex.settings.media]
max_images = 0
max_https_images = 0
max_documents = 0
max_audio = 0
max_videos = 0
max_https_videos = 0
max_inline_image_bytes = 1
max_inline_pdf_bytes = 1
max_inline_text_bytes = 1
url_schemes = ["https"]"#,
    ));
    config.push_str(&model(
        "bedrock",
        r#"provider_id = "amazon.bedrock"
model_id = "arbitrary-bedrock-id"
endpoint = "https://bedrock-runtime.us-east-1.amazonaws.com"
adaptor = "bedrock""#,
        r#"[models.bedrock.auth]
type = "aws_static"
access_key_id = "access"
secret_access_key = "secret"
[models.bedrock.settings]
region = "us-east-1"
reasoning_wire_format = "unsupported"
signed_reasoning = false
structured_output = "unsupported"
max_event_message_bytes = 1048576"#,
    ));
    config.push_str(&model(
        "azure_chat",
        r#"provider_id = "azure.openai"
model_id = "chat-deployment"
endpoint = "https://resource.openai.azure.com"
adaptor = "azure-chat""#,
        r#"[models.azure_chat.auth]
type = "api_key"
value = "azure-secret"
[models.azure_chat.settings]
system_role = "system"
max_tokens_field = "max_tokens"
stream_usage = false
structured_output = "unsupported"
reasoning_field = "none"
omit_reasoning_sampling = false
[models.azure_chat.settings.route]
kind = "v1""#,
    ));
    config.push_str(&model(
        "azure_responses",
        r#"provider_id = "azure.openai"
model_id = "responses-deployment"
endpoint = "https://resource.openai.azure.com"
adaptor = "azure-responses""#,
        r#"[models.azure_responses.auth]
type = "api_key"
value = "azure-secret"
[models.azure_responses.settings]
[models.azure_responses.settings.route]
kind = "v1""#,
    ));
    config.push_str(&model(
        "cohere",
        r#"provider_id = "cohere"
model_id = "arbitrary-cohere-id"
endpoint = "https://example.test/v2/chat"
adaptor = "cohere""#,
        r#"[models.cohere.auth]
type = "bearer"
token = "cohere-secret"
[models.cohere.settings]
strict_tools = false"#,
    ));
    config.push_str(&model(
        "open_responses",
        r#"provider_id = "standard-provider"
model_id = "arbitrary-standard-id"
endpoint = "https://example.test/v1/responses"
adaptor = "open-responses""#,
        r#"[models.open_responses.auth]
type = "bearer"
token = "standard-secret"
[models.open_responses.settings]
strict_json_schema = false
strict_tools = false
parallel_tool_calls = false
store = false
[models.open_responses.settings.transport]
type = "generic"
profile = "standard""#,
    ));
    config.push_str(
        r#"
[agents.primary]
type = "primary"
models = ["anthropic", "openai_chat", "openai_responses", "compatible", "google", "vertex", "bedrock", "azure_chat", "azure_responses", "cohere", "open_responses"]
"#,
    );
    config
}

fn write_config(contents: &str) -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, contents).unwrap();
    (temp, path)
}

#[test]
fn adaptor_discriminator_loads_every_retained_oven_adapter() {
    let source = all_models();
    if let Err(error) = toml::from_str::<cookie_agent_config::Config>(&source) {
        panic!("direct parse failed: {error}");
    }
    let (_temp, path) = write_config(&source);
    let config = load_layered(None, Some(&path)).unwrap();
    let set = config.build_model_set().unwrap();
    assert_eq!(set.aliases().len(), 11);
    assert_eq!(
        set.get("anthropic")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "oven.anthropic.messages"
    );
    assert_eq!(
        set.get("openai_chat")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "oven.openai.chat"
    );
    assert_eq!(
        set.get("openai_responses")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "oven.openai.responses"
    );
    assert_eq!(
        set.get("compatible")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "cookie.gateway.chat"
    );
    assert_eq!(
        set.get("google").unwrap().descriptor().adapter_id.as_str(),
        "oven.google.generate-content"
    );
    assert_eq!(
        set.get("vertex").unwrap().descriptor().adapter_id.as_str(),
        "oven.google.vertex.generate-content"
    );
    assert_eq!(
        set.get("bedrock").unwrap().descriptor().adapter_id.as_str(),
        "oven.bedrock.converse"
    );
    assert_eq!(
        set.get("azure_chat")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "oven.azure.openai.chat"
    );
    assert_eq!(
        set.get("azure_responses")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "oven.azure.openai.responses"
    );
    assert_eq!(
        set.get("cohere").unwrap().descriptor().adapter_id.as_str(),
        "cohere.v2.chat"
    );
    assert_eq!(
        set.get("open_responses")
            .unwrap()
            .descriptor()
            .adapter_id
            .as_str(),
        "open.responses.http_sse"
    );
    assert_eq!(
        set.get("compatible").unwrap().defaults().provider_options["openai_compatible"]["extra_body"]
            ["custom_flag"],
        true
    );

    let policy = config.materialize_policy(&set, "primary").unwrap();
    assert_json_snapshot!("all_model_bindings", policy.models, {
        "[].behavior_fingerprint" => "[behavior fingerprint]",
        "[].configuration_fingerprint" => "[fingerprint]"
    });
}

struct Environment {
    _guard: MutexGuard<'static, ()>,
    prior: BTreeMap<String, Option<OsString>>,
}

impl Environment {
    fn lock() -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        Self {
            _guard: LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            prior: BTreeMap::new(),
        }
    }
    fn set(&mut self, key: &str, value: &str) {
        self.prior.insert(key.into(), std::env::var_os(key));
        unsafe { std::env::set_var(key, value) };
    }

    fn remove(&mut self, key: &str) {
        self.prior.insert(key.into(), std::env::var_os(key));
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in std::mem::take(&mut self.prior) {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[test]
fn interpolation_is_allowed_only_for_endpoint_auth_and_static_header_values() {
    let mut environment = Environment::lock();
    environment.set("OVEN_CONFIG_SECRET", "resolved-secret");
    environment.set("OVEN_CONFIG_HEADER", "resolved-header");
    environment.set("OVEN_CONFIG_ENDPOINT", "https://resolved.example/v1");
    environment.set("OVEN_CONFIG_MODEL", "must-stay-literal");
    let source = all_models()
        .replace("anthropic-secret", "${env:OVEN_CONFIG_SECRET}")
        .replace(
            "endpoint = \"https://example.test/v1\"",
            "endpoint = \"${env:OVEN_CONFIG_ENDPOINT}\"",
        )
        .replace("model_id = \"arbitrary-anthropic-id\"", "model_id = \"${env:OVEN_CONFIG_MODEL}\"\nheaders = { x-test = \"${env:OVEN_CONFIG_HEADER}\" }");
    let (_temp, path) = write_config(&source);
    let config = load_layered(None, Some(&path)).unwrap();
    let set = config.build_model_set().unwrap();
    assert_eq!(
        set.get("anthropic")
            .unwrap()
            .descriptor()
            .identity
            .model_id
            .as_str(),
        "${env:OVEN_CONFIG_MODEL}"
    );
    let serialized = serde_json::to_value(config).unwrap();
    assert_eq!(
        serialized["models"]["anthropic"]["endpoint"],
        "https://resolved.example/v1"
    );
    assert_eq!(
        serialized["models"]["anthropic"]["headers"]["x-test"],
        "resolved-header"
    );
}

#[test]
fn unset_allowed_interpolation_reports_location_without_resolved_secret_values() {
    let mut environment = Environment::lock();
    environment.remove("OVEN_CONFIG_MISSING_SECRET");
    let source = all_models().replace(
        "anthropic-secret",
        "prefix-must-not-leak-${env:OVEN_CONFIG_MISSING_SECRET}",
    );
    let (_temp, path) = write_config(&source);
    let error = load_layered(None, Some(&path)).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("OVEN_CONFIG_MISSING_SECRET"));
    assert!(message.contains("models.anthropic.auth.value"));
    assert!(!message.contains("prefix-must-not-leak"));
    assert!(!format!("{error:?}").contains("prefix-must-not-leak"));
}

#[test]
fn malformed_expressions_remain_literal_and_resolution_is_single_pass() {
    let mut environment = Environment::lock();
    environment.set("OVEN_CONFIG_NESTED", "${env:OVEN_CONFIG_MUST_NOT_BE_READ}");
    environment.remove("OVEN_CONFIG_MUST_NOT_BE_READ");
    let source = all_models()
        .replace("anthropic-secret", "${env:}")
        .replace("openai-secret", "${env:OVEN_CONFIG_NESTED}")
        .replace("gateway-secret", "${env:UNCLOSED");
    let (_temp, path) = write_config(&source);
    let config = load_layered(None, Some(&path)).unwrap();
    let value = serde_json::to_value(config).unwrap();
    assert_eq!(value["models"]["anthropic"]["auth"]["value"], "${env:}");
    assert_eq!(
        value["models"]["openai_chat"]["auth"]["api_key"],
        "${env:OVEN_CONFIG_MUST_NOT_BE_READ}"
    );
    assert_eq!(
        value["models"]["compatible"]["auth"]["token"],
        "${env:UNCLOSED"
    );
}

#[test]
fn environment_variables_never_become_config_keys_but_explicit_interpolation_works() {
    let mut environment = Environment::lock();
    environment.set("COOKIE_TEST_API_KEY", "expected-credential");
    environment.set("COOKIE_AGENT_THEME", "legacy-theme");
    environment.set("COOKIE_AGENT_TEST_API_KEY", "interpolated-secret");
    environment.set("COOKIE_AGENT_FOO", "unrelated-value");
    environment.set("COOKIE_AGENT_CONFIG__SERVER__HOST", "must-not-override");
    environment.set(
        "COOKIE_AGENT_CONFIG__SERVER__PORT",
        "secret-invalid-port-value",
    );
    environment.set(
        "COOKIE_AGENT_CONFIG__SERVER__NOT_A_FIELD",
        "secret-unknown-field-value",
    );
    let source = all_models().replace("anthropic-secret", "${env:COOKIE_AGENT_TEST_API_KEY}");
    let (_temp, path) = write_config(&source);

    let config = load_layered(None, Some(&path)).unwrap();
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 7419);
    assert!(!format!("{config:?}").contains("interpolated-secret"));
    let value = serde_json::to_value(config).unwrap();
    assert_eq!(
        value["models"]["anthropic"]["auth"]["value"],
        "interpolated-secret"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_allowed_interpolation_fails_with_a_redacted_error() {
    use std::os::unix::ffi::OsStringExt as _;

    let mut environment = Environment::lock();
    let key = "OVEN_CONFIG_NON_UTF8";
    environment.prior.insert(key.into(), std::env::var_os(key));
    unsafe { std::env::set_var(key, OsString::from_vec(vec![0xff])) };
    let source = all_models().replace("anthropic-secret", "${env:OVEN_CONFIG_NON_UTF8}");
    let (_temp, path) = write_config(&source);
    let error = load_layered(None, Some(&path)).unwrap_err();
    assert!(error.to_string().contains("OVEN_CONFIG_NON_UTF8"));
    assert!(error.to_string().contains("not valid UTF-8"));
}

#[test]
fn secrets_are_redacted_and_excluded_from_the_safe_fingerprint() {
    let first = all_models();
    let second = first
        .replace("anthropic-secret", "different-secret")
        .replace("cohere-secret", "other-secret");
    let (_first_temp, first_path) = write_config(&first);
    let (_second_temp, second_path) = write_config(&second);
    let first = load_layered(None, Some(&first_path)).unwrap();
    let second = load_layered(None, Some(&second_path)).unwrap();
    assert_eq!(
        first.build_model_set().unwrap().fingerprint(),
        second.build_model_set().unwrap().fingerprint()
    );
    let debug = format!("{first:?}");
    assert!(!debug.contains("anthropic-secret"));
    assert!(!debug.contains("cohere-secret"));
}

#[test]
fn static_header_values_are_excluded_but_header_names_affect_the_fingerprint() {
    let first = all_models();
    let changed_value = first.replace("x-routing = \"route-one\"", "x-routing = \"route-two\"");
    let changed_name = first.replace("x-routing = \"route-one\"", "x-other = \"route-one\"");
    let (_first_temp, first_path) = write_config(&first);
    let (_value_temp, value_path) = write_config(&changed_value);
    let (_name_temp, name_path) = write_config(&changed_name);
    let fingerprint = |path: &PathBuf| {
        load_layered(None, Some(path))
            .unwrap()
            .build_model_set()
            .unwrap()
            .fingerprint()
            .clone()
    };
    assert_eq!(fingerprint(&first_path), fingerprint(&value_path));
    assert_ne!(fingerprint(&first_path), fingerprint(&name_path));
}

#[test]
fn installed_static_models_require_complete_secret_free_behavior_congruence() {
    let original_source = all_models();
    let (_original_temp, original_path) = write_config(&original_source);
    let original = load_layered(None, Some(&original_path)).unwrap();

    for changed in [
        original_source.replace(
            "endpoint = \"https://example.test/v1\"",
            "endpoint = \"https://different.example/v1\"",
        ),
        original_source.replace(
            "adapter_id = \"cookie.gateway.chat\"\nsystem_message_role = \"developer\"\nmax_tokens_field = \"max_completion_tokens\"\nstream_usage = false",
            "adapter_id = \"cookie.gateway.chat\"\nsystem_message_role = \"developer\"\nmax_tokens_field = \"max_completion_tokens\"\nstream_usage = false\nstrict_sse_content_type = true",
        ),
        original_source.replace("x-routing = \"route-one\"", "x-other = \"route-one\""),
        original_source.replace(
            "reject_non_default_sampling = false",
            "reject_non_default_sampling = false\nnative_context_discriminator = \"route-one\"",
        ),
    ] {
        let (_changed_temp, changed_path) = write_config(&changed);
        let installed = load_layered(None, Some(&changed_path))
            .unwrap()
            .build_model_set()
            .unwrap();
        let error = original
            .materialize_policy(&installed, "primary")
            .unwrap_err();
        assert!(matches!(error, ConfigError::InstalledModelSetMismatch));
    }

    let secret_only = original_source
        .replace("anthropic-secret", "rotated-secret")
        .replace("x-routing = \"route-one\"", "x-routing = \"route-two\"");
    let (_secret_temp, secret_path) = write_config(&secret_only);
    let installed = load_layered(None, Some(&secret_path))
        .unwrap()
        .build_model_set()
        .unwrap();
    original.materialize_policy(&installed, "primary").unwrap();
}

#[test]
fn model_ids_do_not_change_adapter_capabilities_or_request_defaults() {
    let first_source = all_models();
    let second_source = first_source.replace("arbitrary-chat-id", "totally-unrelated-name");
    let (_first_temp, first_path) = write_config(&first_source);
    let (_second_temp, second_path) = write_config(&second_source);
    let first = load_layered(None, Some(&first_path))
        .unwrap()
        .build_model_set()
        .unwrap();
    let second = load_layered(None, Some(&second_path))
        .unwrap()
        .build_model_set()
        .unwrap();
    let first = first.get("openai_chat").unwrap();
    let second = second.get("openai_chat").unwrap();
    assert_eq!(
        first.descriptor().adapter_id,
        second.descriptor().adapter_id
    );
    assert_eq!(
        first.descriptor().capabilities,
        second.descriptor().capabilities
    );
    assert_eq!(first.defaults(), second.defaults());
    assert_ne!(
        first.descriptor().identity.model_id,
        second.descriptor().identity.model_id
    );
}

#[test]
fn old_provider_objects_inline_agent_models_and_unknown_fields_are_rejected() {
    let cases = [
        "[providers.old]\ntype = \"anthropic\"",
        "[agents.primary]\ntype = \"primary\"\nmodels = [{ provider = \"old\", model = \"x\" }]",
        "[server]\nhost = \"127.0.0.1\"\nlegacy = true",
        "[models.bad]\nprovider_id = \"x\"\nmodel_id = \"x\"\nendpoint = \"https://example.test\"\nadaptor = \"openai-compatible\"\n[models.bad.auth]\ntype = \"none\"\n[models.bad.capabilities]\nfeatures = []\ncancellation = \"local_only\"\nlegacy = true",
    ];
    for source in cases {
        let (_temp, path) = write_config(source);
        assert!(matches!(
            load_layered(None, Some(&path)),
            Err(ConfigError::Extract { .. })
        ));
    }
}

#[test]
fn old_provider_discriminator_is_rejected_as_unknown() {
    let source = all_models().replacen("adaptor = \"anthropic\"", "provider = \"anthropic\"", 1);
    let error = toml::from_str::<cookie_agent_config::Config>(&source).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown configured model field `provider`")
    );

    let (_temp, path) = write_config(&source);
    assert!(matches!(
        load_layered(None, Some(&path)),
        Err(ConfigError::Extract { .. })
    ));
}

#[test]
fn agent_chains_are_alias_only_and_unknown_aliases_fail_validation() {
    let source = all_models().replace("\"open_responses\"]", "\"missing\"]");
    let (_temp, path) = write_config(&source);
    let error = load_layered(None, Some(&path)).unwrap_err();
    assert!(error.to_string().contains("unknown model alias `missing`"));
}

#[test]
fn checked_in_workspace_config_is_a_valid_schema_v5_daemon_fixture() {
    let mut environment = Environment::lock();
    environment.set("COOKIE_TEST_API_KEY", "fixture-secret");
    environment.set("COOKIE_THEME", "high-contrast");
    environment.set("NO_COLOR", "1");
    environment.set("TERM", "xterm-256color");
    environment.set("COLORTERM", "truecolor");
    let home = TempDir::new().unwrap();
    environment.set("HOME", home.path().to_str().unwrap());
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let config = load(&workspace).unwrap();
    assert_eq!(config.schema_version, 5);
    let set = config.build_model_set().unwrap();
    assert_eq!(
        set.aliases().collect::<Vec<_>>(),
        ["anthropic_live", "chat_live", "responses_live"]
    );
    let responses = &set.get("responses_live").unwrap().descriptor().capabilities;
    assert_eq!(responses.compaction, oven_sdk::CompactionCapability::Native);
    assert_eq!(responses.replay.policy, oven_sdk::ReplayPolicy::IfValid);
    assert_eq!(
        responses.replay.capability,
        oven_sdk::ReplayCapability::Required
    );
    assert!(responses.replay.reasoning);
    for profile in ["primary", "anthropic", "responses", "chat"] {
        let delegation = &config.agents[profile].delegation;
        assert!(delegation.enabled);
        assert_eq!(delegation.allowed_profiles, ["worker"]);
        assert_eq!(delegation.limit, Some(3));
    }

    fn last_rule<'a>(
        policy: &'a PolicySnapshot,
        action: &str,
        resource: &str,
    ) -> (&'a str, &'a str) {
        policy
            .permissions
            .rules
            .iter()
            .rev()
            .find(|rule| rule.action == action && simple_wildcard_match(&rule.resource, resource))
            .map(|rule| (rule.id.as_str(), rule.effect.as_str()))
            .unwrap()
    }

    let policy = config.materialize_policy(&set, "primary").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let ordinary = workspace.join("crates/config/src/lib.rs");
    let root_env = workspace.join(".env");
    let env_example = workspace.join(".env.example");
    let nested_env = workspace.join("nested/.env.local");
    let store = workspace.join("credentials/store-v1.json");
    let token = workspace.join("daemon/token-v1");
    let credential_file = workspace.join("config/credentials.json");
    let secret_file = workspace.join("config/secrets.toml");
    let generic_token = workspace.join("config/token.txt");
    assert_eq!(
        last_rule(&policy, "read", &ordinary.to_string_lossy()),
        ("allow-workspace-read", "allow")
    );
    for protected in [
        &root_env,
        &nested_env,
        &store,
        &token,
        &credential_file,
        &secret_file,
        &generic_token,
    ] {
        assert_eq!(
            last_rule(&policy, "read", &protected.to_string_lossy()).1,
            "deny"
        );
    }
    assert_eq!(
        last_rule(&policy, "read", &env_example.to_string_lossy()),
        ("allow-read-dotenv-example", "allow")
    );
    assert_eq!(
        last_rule(&policy, "grep", &root_env.to_string_lossy()),
        ("deny-workspace-search-enumeration", "deny")
    );
    assert_eq!(
        last_rule(&policy, "glob", &nested_env.to_string_lossy()),
        ("deny-workspace-glob-enumeration", "deny")
    );

    let user = home.path().join("user.toml");
    fs::write(
        &user,
        "[[permissions.rules]]\nid = \"user-deny-read\"\naction = \"read\"\nresource = \"*\"\neffect = \"deny\"\n",
    )
    .unwrap();
    let layered = load_layered(
        Some(&user),
        Some(&workspace.join(".cookie_agent/config.toml")),
    )
    .unwrap();
    let layered_set = layered.build_model_set().unwrap();
    let layered_policy = layered.materialize_policy(&layered_set, "primary").unwrap();
    assert_eq!(
        last_rule(&layered_policy, "read", &root_env.to_string_lossy()),
        ("deny-read-dotenv", "deny")
    );
}

#[test]
fn responses_compaction_is_explicit_and_old_discriminator_names_are_rejected() {
    let openai = model(
        "responses",
        r#"provider_id = "openai"
model_id = "gpt-5.4"
endpoint = "https://api.openai.com/v1"
adaptor = "openai-responses""#,
        r#"[models.responses.auth]
type = "openai"
api_key = "secret"
[models.responses.settings]
compaction = "v1""#,
    )
    .replace("compaction = \"unsupported\"", "compaction = \"native\"");
    let source =
        format!("{openai}\n[agents.primary]\ntype = \"primary\"\nmodels = [\"responses\"]");
    let (_temp, path) = write_config(&source);
    let config = load_layered(None, Some(&path)).unwrap();
    let entry = config
        .build_model_set()
        .unwrap()
        .get("responses")
        .unwrap()
        .clone();
    assert_eq!(
        entry.descriptor().capabilities.compaction,
        oven_sdk::CompactionCapability::Native
    );

    let azure = model(
        "azure",
        r#"provider_id = "azure.openai"
model_id = "responses-deployment"
endpoint = "https://resource.openai.azure.com"
adaptor = "azure-responses""#,
        r#"[models.azure.auth]
type = "api_key"
value = "secret"
[models.azure.settings]
revision = { model = "gpt-5.4", version = "2026-01-01", deployment_type = "standard" }
compaction = { kind = "v1", routing_discriminator = "resource-one" }
[models.azure.settings.route]
kind = "v1""#,
    )
    .replace("compaction = \"unsupported\"", "compaction = \"native\"");
    let source = format!("{azure}\n[agents.primary]\ntype = \"primary\"\nmodels = [\"azure\"]");
    let (_temp, path) = write_config(&source);
    let config = load_layered(None, Some(&path)).unwrap();
    assert_eq!(
        config
            .build_model_set()
            .unwrap()
            .get("azure")
            .unwrap()
            .descriptor()
            .capabilities
            .compaction,
        oven_sdk::CompactionCapability::Native
    );

    let anthropic = model(
        "anthropic",
        r#"provider_id = "anthropic"
model_id = "claude-opus-4-6"
endpoint = "https://api.anthropic.com/v1"
adaptor = "anthropic""#,
        r#"[models.anthropic.auth]
type = "api_key"
value = "secret"
[models.anthropic.settings]
thinking = "none"
thinking_default_active = false
thinking_disable_allowed = false
effort = false
assistant_prefill = false
reject_non_default_sampling = false
replay_discriminator = "old""#,
    );
    let (_temp, path) = write_config(&anthropic);
    assert!(matches!(
        load_layered(None, Some(&path)),
        Err(ConfigError::Extract { .. })
    ));
}
