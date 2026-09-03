use std::fs;

use cookie_agent_config::{
    AgentFrontmatter, CacheTtl, ConfigError, OpenAiCacheMode, OpenAiPromptCacheRetention,
    OpenAiPromptCacheTtl, RollingCacheTtl, load_from_roots,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

fn load(text: &str) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("config.toml"), text).unwrap();
    load_from_roots(None, Some(directory.path()))
}

fn custom_provider(adaptor: &str, auth: &str, setup: &str, cache: &str) -> String {
    format!(
        r#"
[providers.test]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "{adaptor}"
setup = {setup}
auth = {auth}

[providers.test.cache]
{cache}

[providers.test.models.model]
display_name = "Model"

[providers.test.models.model.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 4096
output_tokens = 1024
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = false
native_replay = "unsupported"
cancellation = "local_only"
media = {{}}
"#
    )
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Ttls {
    system: CacheTtl,
    rolling: RollingCacheTtl,
}

#[test]
fn cache_ttl_literals_round_trip_exactly() {
    for (system, rolling) in [("1h", "5m"), ("5m", "off"), ("off", "5m")] {
        let value: Ttls =
            toml::from_str(&format!("system = \"{system}\"\nrolling = \"{rolling}\"\n")).unwrap();
        let encoded = toml::to_string(&value).unwrap();
        assert!(encoded.contains(&format!("system = \"{system}\"")));
        assert!(encoded.contains(&format!("rolling = \"{rolling}\"")));
    }
}

#[test]
fn removed_cache_ttl_literals_are_hard_errors() {
    for literal in [
        "one_hour",
        "five_minutes",
        "short",
        "long",
        "standard",
        "none",
        "ephemeral",
        "explicit",
    ] {
        assert!(
            toml::from_str::<Ttls>(&format!("system = \"{literal}\"\nrolling = \"off\"\n"))
                .is_err(),
            "accepted removed TTL literal {literal}"
        );
    }
}

#[test]
fn global_prompt_caching_section_is_removed() {
    for text in [
        "[prompt_caching]\n",
        "[prompt_caching.anthropic]\nsystem = \"1h\"\n",
    ] {
        assert!(
            load(text).is_err(),
            "accepted removed global config: {text}"
        );
    }
}

#[test]
fn per_provider_cache_tables_parse_for_their_adaptor_family() {
    let cases = [
        (
            "anthropic-compatible",
            r#"{ method = "anthropic-api-key-v1", values = { api_key = "key" } }"#,
            "{}",
            "system = \"1h\"\ntools = \"1h\"\nrolling = \"5m\"",
        ),
        (
            "aws-bedrock-converse",
            r#"{ method = "aws-sigv4-credentials-v1", values = { access_key_id = "id", secret_access_key = "secret" } }"#,
            r#"{ region = "us-east-1" }"#,
            "system = \"5m\"\ntools = \"off\"\nrolling = \"5m\"",
        ),
        (
            "openai-chat",
            r#"{ method = "no-auth-v1", values = {} }"#,
            "{}",
            "mode = \"auto\"\nttl = \"30m\"\nsystem = true",
        ),
        (
            "openai-compatible",
            r#"{ method = "no-auth-v1", values = {} }"#,
            "{}",
            "prompt_cache_key = \"tenant-${session_id}\"",
        ),
    ];
    for (adaptor, auth, setup, cache) in cases {
        let loaded = load(&custom_provider(adaptor, auth, setup, cache))
            .unwrap_or_else(|error| panic!("{adaptor} provider cache failed: {error}"));
        assert_eq!(loaded.runtime.providers.len(), 1);
    }
}

#[test]
fn per_provider_cache_tables_reject_wrong_families_and_removed_surfaces() {
    let no_auth = r#"{ method = "no-auth-v1", values = {} }"#;
    for (adaptor, cache) in [
        ("openai-chat", "system = \"1h\""),
        ("openai-compatible", "mode = \"implicit\""),
        ("google-gemini", ""),
        ("openai-chat", "prompt_cache_key = \"disabled\""),
        ("openai-chat", "mode = \"implicit\""),
        ("aws-bedrock-converse", "enabled = false"),
        ("aws-bedrock-converse", "messages = []"),
    ] {
        let (auth, setup) = match adaptor {
            "google-gemini" => (
                r#"{ method = "google-api-key-header-v1", values = { api_key = "key" } }"#,
                "{}",
            ),
            "aws-bedrock-converse" => (
                r#"{ method = "aws-sigv4-credentials-v1", values = { access_key_id = "id", secret_access_key = "secret" } }"#,
                r#"{ region = "us-east-1" }"#,
            ),
            _ => (no_auth, "{}"),
        };
        assert!(
            load(&custom_provider(adaptor, auth, setup, cache)).is_err(),
            "accepted invalid {adaptor} provider cache: {cache}"
        );
    }
}

#[test]
fn provider_cache_errors_preserve_provider_field_and_literal_context() {
    let error = load(&custom_provider(
        "openai-chat",
        r#"{ method = "no-auth-v1", values = {} }"#,
        "{}",
        "mode = \"implicit\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("provider `test`"), "{error}");
    assert!(error.contains("mode"), "{error}");
    assert!(error.contains("implicit"), "{error}");
    assert!(error.contains("auto"), "{error}");
}

#[test]
fn agent_model_cache_uses_strict_current_family_shapes() {
    let frontmatter: AgentFrontmatter = serde_yaml::from_str(
        r#"
description: Cached agent
mode: primary
enabled: true
models:
  - model: custom.test/model
    cache:
      anthropic:
        system: "1h"
        tools: "1h"
        rolling: "5m"
  - model: custom.test/other
    cache:
      openai:
        prompt_cache_retention: in_memory
        mode: explicit
        ttl: 30m
        system: true
        rolling: true
permissions: {}
"#,
    )
    .unwrap();
    let anthropic = frontmatter.models[0]
        .cache
        .as_ref()
        .unwrap()
        .anthropic
        .as_ref()
        .unwrap();
    assert_eq!(anthropic.system, CacheTtl::OneHour);
    assert_eq!(anthropic.rolling, RollingCacheTtl::FiveMinutes);
    let openai = frontmatter.models[1]
        .cache
        .as_ref()
        .unwrap()
        .openai
        .as_ref()
        .unwrap();
    assert_eq!(
        openai.prompt_cache_retention,
        Some(OpenAiPromptCacheRetention::InMemory)
    );
    assert_eq!(openai.mode, Some(OpenAiCacheMode::Explicit));
    assert_eq!(openai.ttl, Some(OpenAiPromptCacheTtl::ThirtyMinutes));
}

#[test]
fn removed_binding_cache_surfaces_are_hard_errors() {
    for invalid in [
        "google: {}",
        "anthropic: { system: one_hour }",
        "bedrock: { enabled: false }",
        "bedrock: { messages: [] }",
        "openai: { prompt_cache_key: session }",
        "unknown: {}",
    ] {
        let yaml = format!(
            "description: Cached agent\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/model\n    cache: {{ {invalid} }}\npermissions: {{}}\n"
        );
        assert!(
            serde_yaml::from_str::<AgentFrontmatter>(&yaml).is_err(),
            "accepted removed binding cache shape: {invalid}"
        );
    }
}

#[test]
fn bedrock_uses_anthropic_ordering_rules() {
    let yaml = "description: Cached agent\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/model\n    cache:\n      bedrock: { tools: 5m, system: 1h, rolling: 5m }\npermissions: {}\n";
    assert!(serde_yaml::from_str::<AgentFrontmatter>(yaml).is_err());
}
