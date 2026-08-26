pub use oven_sdk_bedrock::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
};

use super::AnthropicCacheStrategyConfig;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCacheStrategyConfig {
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<OpenAiPromptCacheRetention>,
}
