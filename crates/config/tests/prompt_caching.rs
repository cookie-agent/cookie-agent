use std::fs;

use cookie_agent_config::{
    AgentFrontmatter, BedrockCacheTtl, CacheTtl, ConfigError, GoogleCacheMode, OpenAiCacheMode,
    OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, RollingCacheTtl, load_from_roots,
};
use cookie_agent_models::adapters::AnthropicCacheTtlConfig;
use tempfile::TempDir;

fn load(text: &str) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("config.toml"), text).unwrap();
    load_from_roots(None, Some(directory.path()))
}

#[test]
fn prompt_caching_defaults_to_anthropic_ordered_markers() {
    let loaded = load("").unwrap();
    let caching = loaded.runtime.prompt_caching;
    let anthropic = caching.anthropic.as_ref().unwrap();
    assert_eq!(anthropic.system, CacheTtl::OneHour);
    assert_eq!(anthropic.tools, CacheTtl::OneHour);
    assert_eq!(anthropic.rolling, RollingCacheTtl::FiveMinutes);
    let strategy = caching.strategy().unwrap();
    assert_eq!(strategy.system, Some(AnthropicCacheTtlConfig::OneHour));
    assert_eq!(strategy.tools, Some(AnthropicCacheTtlConfig::OneHour));
    assert_eq!(strategy.rolling, Some(AnthropicCacheTtlConfig::FiveMinutes));
    assert!(caching.bedrock.is_none());
    assert!(caching.google.is_none());
    assert!(caching.openai.is_none());
}

#[test]
fn runtime_cache_sections_parse_strict_provider_shapes() {
    let loaded = load(
        r#"
[prompt_caching.anthropic]
system = "off"
tools = "one_hour"
rolling = "five_minutes"

[prompt_caching.bedrock]
enabled = true
system = "one_hour"
tools = "one_hour"

[[prompt_caching.bedrock.messages]]
history_index = 2
ttl = "five_minutes"

[prompt_caching.google]
mode = "explicit"
cached_content = "cachedContents/runtime"

[prompt_caching.openai]
prompt_cache_key = "runtime-${session_id}"
prompt_cache_retention = "24h"
mode = "explicit"
ttl = "30m"
system = true
rolling = true
"#,
    )
    .unwrap();
    let caching = loaded.runtime.prompt_caching;
    assert_eq!(caching.anthropic.unwrap().system, CacheTtl::Off);
    assert_eq!(
        caching.bedrock.unwrap().messages.unwrap()[0].ttl,
        BedrockCacheTtl::FiveMinutes
    );
    assert_eq!(caching.google.unwrap().mode, GoogleCacheMode::Explicit);
    assert_eq!(
        caching.openai.as_ref().unwrap().prompt_cache_retention,
        Some(OpenAiPromptCacheRetention::TwentyFourHours)
    );
    let openai = caching.openai.unwrap();
    assert_eq!(openai.mode, Some(OpenAiCacheMode::Explicit));
    assert_eq!(openai.ttl, Some(OpenAiPromptCacheTtl::ThirtyMinutes));
    assert!(openai.system);
    assert!(openai.rolling);
}

#[test]
fn runtime_cache_sections_reject_unknown_and_incoherent_options() {
    for text in [
        "[prompt_caching.unknown]\nenabled = true\n",
        "[prompt_caching.anthropic]\ntools = \"five_minutes\"\nsystem = \"one_hour\"\n",
        "[prompt_caching.google]\nmode = \"explicit\"\n",
        "[prompt_caching.google]\nmode = \"off\"\ncached_content = \"cachedContents/x\"\n",
        &format!(
            "[prompt_caching.openai]\nprompt_cache_key = \"{}\"\n",
            "x".repeat(65)
        ),
        "[prompt_caching.openai]\nmode = \"future\"\n",
        "[prompt_caching.openai]\nttl = \"1h\"\n",
        "[prompt_caching.openai]\ntools = true\n",
        "[prompt_caching.bedrock]\nenabled = false\nsystem = \"one_hour\"\n",
        "[prompt_caching.bedrock]\ntools = \"five_minutes\"\nsystem = \"one_hour\"\n",
        "[prompt_caching.bedrock]\nmessages = [{ history_index = 1, ttl = \"five_minutes\" }, { history_index = 1, ttl = \"five_minutes\" }]\n",
        "[prompt_caching.bedrock]\nmessages = [{ history_index = 0, ttl = \"one_hour\" }, { history_index = 1, ttl = \"one_hour\" }, { history_index = 2, ttl = \"one_hour\" }]\n",
    ] {
        assert!(load(text).is_err(), "accepted invalid cache config: {text}");
    }
}

#[test]
fn agent_model_cache_is_optional_strict_and_validated() {
    let frontmatter: AgentFrontmatter = serde_yaml::from_str(
        r#"
description: Cached agent
mode: primary
enabled: true
models:
  - model: custom.test/model
    variant: null
    cache:
      openai:
        prompt_cache_key: agent-${session_id}
        prompt_cache_retention: in_memory
permissions: {}
"#,
    )
    .unwrap();
    assert_eq!(
        frontmatter.models[0]
            .cache
            .as_ref()
            .unwrap()
            .openai
            .as_ref()
            .unwrap()
            .prompt_cache_retention,
        Some(OpenAiPromptCacheRetention::InMemory)
    );
    let openai = frontmatter.models[0]
        .cache
        .as_ref()
        .unwrap()
        .openai
        .as_ref()
        .unwrap();
    assert!(!openai.gpt_5_6_controls_enabled());
    assert_eq!(openai.effective_mode(), OpenAiCacheMode::Implicit);
    assert_eq!(openai.effective_ttl(), OpenAiPromptCacheTtl::ThirtyMinutes);

    let gpt_5_6: AgentFrontmatter = serde_yaml::from_str(
        r#"
description: GPT cache agent
mode: primary
enabled: true
models:
  - model: custom.test/model
    variant: null
    cache:
      openai:
        mode: explicit
        ttl: 30m
        system: true
        rolling: true
permissions: {}
"#,
    )
    .unwrap();
    let openai = gpt_5_6.models[0]
        .cache
        .as_ref()
        .unwrap()
        .openai
        .as_ref()
        .unwrap();
    assert!(openai.gpt_5_6_controls_enabled());
    assert_eq!(openai.effective_mode(), OpenAiCacheMode::Explicit);

    for invalid in [
        "unknown: {}",
        "google: { mode: explicit }",
        "anthropic: { tools: five_minutes, system: one_hour }",
    ] {
        let yaml = format!(
            "description: Cached agent\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/model\n    variant: null\n    cache: {{ {invalid} }}\npermissions: {{}}\n"
        );
        assert!(serde_yaml::from_str::<AgentFrontmatter>(&yaml).is_err());
    }
}
