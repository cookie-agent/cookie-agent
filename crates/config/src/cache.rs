use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    OneHour,
    FiveMinutes,
    Off,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingCacheTtl {
    FiveMinutes,
    Off,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicCacheConfig {
    #[serde(default = "one_hour")]
    pub system: CacheTtl,
    #[serde(default = "one_hour")]
    pub tools: CacheTtl,
    #[serde(default = "rolling_five_minutes")]
    pub rolling: RollingCacheTtl,
}

impl Default for AnthropicCacheConfig {
    fn default() -> Self {
        Self {
            system: one_hour(),
            tools: one_hour(),
            rolling: rolling_five_minutes(),
        }
    }
}

impl AnthropicCacheConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_ttl_order([
            ttl(self.tools),
            ttl(self.system),
            match self.rolling {
                RollingCacheTtl::FiveMinutes => Some(CacheTtl::FiveMinutes),
                RollingCacheTtl::Off => None,
            },
        ])
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BedrockCacheTtl {
    OneHour,
    FiveMinutes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockMessageCacheConfig {
    pub history_index: usize,
    pub ttl: BedrockCacheTtl,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockCacheConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub system: Option<CacheTtl>,
    pub tools: Option<CacheTtl>,
    pub messages: Option<Vec<BedrockMessageCacheConfig>>,
}

impl Default for BedrockCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            system: None,
            tools: None,
            messages: None,
        }
    }
}

impl BedrockCacheConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.enabled
            && (self.system.is_some() || self.tools.is_some() || self.messages.is_some())
        {
            return Err("disabled Bedrock caching cannot declare placements");
        }
        if !self.enabled {
            return Ok(());
        }
        let default_system = self.system.unwrap_or(CacheTtl::FiveMinutes);
        let default_tools = self.tools.unwrap_or(CacheTtl::FiveMinutes);
        let default_message = [BedrockMessageCacheConfig {
            history_index: usize::MAX,
            ttl: BedrockCacheTtl::FiveMinutes,
        }];
        let messages = self.messages.as_deref().unwrap_or(&default_message);
        let count = usize::from(default_system != CacheTtl::Off)
            + usize::from(default_tools != CacheTtl::Off)
            + messages.len();
        if count > 4 {
            return Err("Bedrock allows at most four cache points");
        }
        let mut indices = BTreeSet::new();
        if messages
            .iter()
            .any(|message| !indices.insert(message.history_index))
        {
            return Err("Bedrock message cache indices must be unique");
        }
        validate_ttl_order([ttl(default_tools), ttl(default_system)].into_iter().chain(
            messages.iter().map(|message| {
                Some(match message.ttl {
                    BedrockCacheTtl::OneHour => CacheTtl::OneHour,
                    BedrockCacheTtl::FiveMinutes => CacheTtl::FiveMinutes,
                })
            }),
        ))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoogleCacheMode {
    #[default]
    Implicit,
    Explicit,
    Off,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleCacheConfig {
    #[serde(default)]
    pub mode: GoogleCacheMode,
    pub cached_content: Option<String>,
}

impl GoogleCacheConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if (self.mode == GoogleCacheMode::Explicit) != self.cached_content.is_some() {
            return Err("Google cached_content is required if and only if mode is explicit");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiPromptCacheRetention {
    InMemory,
    #[serde(rename = "24h")]
    TwentyFourHours,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCacheMode {
    #[default]
    Implicit,
    Explicit,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum OpenAiPromptCacheTtl {
    #[default]
    #[serde(rename = "30m")]
    ThirtyMinutes,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiCacheConfig {
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<OpenAiPromptCacheRetention>,
    pub mode: Option<OpenAiCacheMode>,
    pub ttl: Option<OpenAiPromptCacheTtl>,
    #[serde(default)]
    pub system: bool,
    #[serde(default)]
    pub rolling: bool,
}

impl OpenAiCacheConfig {
    #[must_use]
    pub const fn gpt_5_6_controls_enabled(&self) -> bool {
        self.mode.is_some() || self.ttl.is_some() || self.system || self.rolling
    }

    #[must_use]
    pub fn effective_mode(&self) -> OpenAiCacheMode {
        self.mode.unwrap_or_default()
    }

    #[must_use]
    pub fn effective_ttl(&self) -> OpenAiPromptCacheTtl {
        self.ttl.unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .prompt_cache_key
            .as_ref()
            .is_some_and(|key| key.chars().count() > 64)
        {
            return Err("OpenAI prompt_cache_key must not exceed 64 characters");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CacheConfig {
    pub anthropic: Option<AnthropicCacheConfig>,
    pub bedrock: Option<BedrockCacheConfig>,
    pub google: Option<GoogleCacheConfig>,
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
        if let Some(config) = &self.google {
            config.validate()?;
        }
        if let Some(config) = &self.openai {
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
            google: Option<GoogleCacheConfig>,
            openai: Option<OpenAiCacheConfig>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let config = Self {
            anthropic: raw.anthropic,
            bedrock: raw.bedrock,
            google: raw.google,
            openai: raw.openai,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

const fn ttl(value: CacheTtl) -> Option<CacheTtl> {
    match value {
        CacheTtl::OneHour | CacheTtl::FiveMinutes => Some(value),
        CacheTtl::Off => None,
    }
}

fn validate_ttl_order(
    values: impl IntoIterator<Item = Option<CacheTtl>>,
) -> Result<(), &'static str> {
    let mut saw_five_minutes = false;
    for value in values.into_iter().flatten() {
        match value {
            CacheTtl::FiveMinutes => saw_five_minutes = true,
            CacheTtl::OneHour if saw_five_minutes => {
                return Err("one-hour cache points must precede five-minute cache points");
            }
            CacheTtl::OneHour | CacheTtl::Off => {}
        }
    }
    Ok(())
}

const fn one_hour() -> CacheTtl {
    CacheTtl::OneHour
}

const fn rolling_five_minutes() -> RollingCacheTtl {
    RollingCacheTtl::FiveMinutes
}

const fn yes() -> bool {
    true
}
