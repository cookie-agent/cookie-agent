pub use oven_sdk_bedrock::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicCacheTtlConfig {
    FiveMinutes,
    OneHour,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AnthropicCacheStrategyConfig {
    pub system: Option<AnthropicCacheTtlConfig>,
    pub tools: Option<AnthropicCacheTtlConfig>,
    pub rolling: Option<AnthropicCacheTtlConfig>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CacheStrategyConfig {
    Anthropic(AnthropicCacheStrategyConfig),
    Bedrock(BedrockCacheStrategy),
    Google(GoogleCacheStrategyConfig),
    OpenAi(OpenAiCacheStrategyConfig),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoogleCacheMode {
    Implicit,
    Explicit,
    Off,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleCacheStrategyConfig {
    pub mode: GoogleCacheMode,
    pub cached_content: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiPromptCacheRetention {
    InMemory,
    TwentyFourHours,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiCacheMode {
    Implicit,
    Explicit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiPromptCacheTtl {
    ThirtyMinutes,
    TwelveHours,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCacheStrategyConfig {
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<OpenAiPromptCacheRetention>,
    pub mode: Option<OpenAiCacheMode>,
    pub ttl: Option<OpenAiPromptCacheTtl>,
    pub system: bool,
    pub rolling: bool,
}

/// Whether the Bedrock model `wire_model_id` takes a one-hour cache TTL.
/// Bedrock lists the TTLs each model supports
/// (docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html): the
/// Claude models from Claude Sonnet 4.5, Haiku 4.5, and Opus 4.5 on take
/// `5m` and `1h`, while older Claude models and other families take only
/// `5m` and may reject a `ttl` with a `ValidationException`. Bedrock
/// publishes no capability for it, so it is read from the model ID: a
/// Claude ID of version 4.5 or later in any region or profile prefix
/// (`us.anthropic.claude-sonnet-4-5-20250929-v1:0`,
/// `anthropic.claude-opus-5`). Any other ID, an application inference
/// profile ARN among them, counts as unsupported.
#[must_use]
pub fn bedrock_supports_one_hour_cache(wire_model_id: &str) -> bool {
    let id = wire_model_id.to_ascii_lowercase();
    let Some((_, rest)) = id.split_once("anthropic.claude-") else {
        return false;
    };
    let mut parts = rest.split(['-', '.', ':']);
    // `claude-3-7-sonnet-...` names the version before the family.
    let Some(family) = parts
        .next()
        .filter(|family| !family.starts_with(|c: char| c.is_ascii_digit()))
    else {
        return false;
    };
    if family.is_empty() {
        return false;
    }
    let Some(major) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
        return false;
    };
    // A date (`20250929`) or a version suffix (`v1`) is not a minor version.
    let minor = parts
        .next()
        .filter(|part| part.len() <= 2)
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    (major, minor) >= (4, 5)
}

#[cfg(test)]
mod tests {
    use super::bedrock_supports_one_hour_cache;

    #[test]
    fn bedrock_one_hour_cache_follows_the_claude_version() {
        for supported in [
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "global.anthropic.claude-opus-4-5-20251101-v1:0",
            "anthropic.claude-opus-4-6-v1",
            "eu.anthropic.claude-sonnet-4-6",
            "anthropic.claude-opus-5",
            "anthropic.claude-sonnet-5-5",
            "anthropic.claude-fable-5-1",
            "Anthropic.Claude-Mythos-5",
        ] {
            assert!(bedrock_supports_one_hour_cache(supported), "{supported}");
        }
        for unsupported in [
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "anthropic.claude-opus-4-1-20250805-v1:0",
            "amazon.nova-pro-v1:0",
            "openai.gpt-5.6-sol",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123",
            "claude-sonnet-4-5",
            "anthropic.claude-",
        ] {
            assert!(
                !bedrock_supports_one_hour_cache(unsupported),
                "{unsupported}"
            );
        }
    }
}
