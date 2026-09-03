use serde::{Deserialize, Serialize};

pub use cookie_agent_models::{
    AnthropicCacheConfig, BedrockCacheConfig, CacheTtl, OpenAiCacheConfig, OpenAiCacheMode,
    OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, RollingCacheTtl,
};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CacheConfig {
    pub anthropic: Option<AnthropicCacheConfig>,
    pub bedrock: Option<BedrockCacheConfig>,
    pub openai: Option<OpenAiCacheConfig>,
}

impl CacheConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(config) = &self.anthropic {
            config.validate()?;
        }
        if let Some(config) = &self.bedrock {
            config.validate()?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CacheConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            anthropic: Option<AnthropicCacheConfig>,
            bedrock: Option<BedrockCacheConfig>,
            openai: Option<OpenAiCacheConfig>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let config = Self {
            anthropic: raw.anthropic,
            bedrock: raw.bedrock,
            openai: raw.openai,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}
