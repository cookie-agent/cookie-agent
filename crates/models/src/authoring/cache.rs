use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CacheTtl {
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "off")]
    Off,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RollingCacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "off")]
    Off,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicCacheConfig {
    pub system: CacheTtl,
    pub tools: CacheTtl,
    pub rolling: RollingCacheTtl,
    #[serde(skip)]
    explicitly_requests_one_hour: bool,
}

impl Default for AnthropicCacheConfig {
    fn default() -> Self {
        Self {
            system: CacheTtl::OneHour,
            tools: CacheTtl::OneHour,
            rolling: RollingCacheTtl::FiveMinutes,
            explicitly_requests_one_hour: false,
        }
    }
}

impl<'de> Deserialize<'de> for AnthropicCacheConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            system: Option<CacheTtl>,
            tools: Option<CacheTtl>,
            rolling: Option<RollingCacheTtl>,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            explicitly_requests_one_hour: raw.system == Some(CacheTtl::OneHour)
                || raw.tools == Some(CacheTtl::OneHour),
            system: raw.system.unwrap_or(CacheTtl::OneHour),
            tools: raw.tools.unwrap_or(CacheTtl::OneHour),
            rolling: raw.rolling.unwrap_or(RollingCacheTtl::FiveMinutes),
        })
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

    #[must_use]
    pub const fn explicitly_requests_one_hour(&self) -> bool {
        self.explicitly_requests_one_hour
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockCacheConfig {
    #[serde(default = "five_minutes")]
    pub system: CacheTtl,
    #[serde(default = "five_minutes")]
    pub tools: CacheTtl,
    #[serde(default = "rolling_five_minutes")]
    pub rolling: RollingCacheTtl,
}

impl Default for BedrockCacheConfig {
    fn default() -> Self {
        Self {
            system: five_minutes(),
            tools: five_minutes(),
            rolling: rolling_five_minutes(),
        }
    }
}

impl BedrockCacheConfig {
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
pub enum OpenAiPromptCacheRetention {
    InMemory,
    #[serde(rename = "24h")]
    TwentyFourHours,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCacheMode {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "explicit")]
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
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiCompatibleCacheConfig {
    pub prompt_cache_key: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderCacheConfig(serde_json::Value);

impl<'de> Deserialize<'de> for ProviderCacheConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct KnownFields {
            system: Option<serde_json::Value>,
            tools: Option<serde_json::Value>,
            rolling: Option<serde_json::Value>,
            prompt_cache_retention: Option<serde_json::Value>,
            mode: Option<serde_json::Value>,
            ttl: Option<serde_json::Value>,
            prompt_cache_key: Option<serde_json::Value>,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        let fields = KnownFields::deserialize(value.clone()).map_err(serde::de::Error::custom)?;
        let _ = (
            fields.system,
            fields.tools,
            fields.rolling,
            fields.prompt_cache_retention,
            fields.mode,
            fields.ttl,
            fields.prompt_cache_key,
        );
        Ok(Self(value))
    }
}

impl ProviderCacheConfig {
    pub fn validate_supported_shape(&self) -> Result<(), String> {
        let object = self
            .0
            .as_object()
            .expect("provider cache deserialization requires an object");
        if object.is_empty() {
            return Ok(());
        }
        if object.contains_key("prompt_cache_key") {
            return self.openai_compatible().map(|_| ());
        }
        if object.contains_key("mode")
            || object.contains_key("ttl")
            || object.contains_key("prompt_cache_retention")
            || object
                .get("system")
                .is_some_and(serde_json::Value::is_boolean)
            || object
                .get("rolling")
                .is_some_and(serde_json::Value::is_boolean)
        {
            return self.openai().map(|_| ());
        }
        self.anthropic().map(|_| ())
    }

    pub fn anthropic(&self) -> Result<AnthropicCacheConfig, String> {
        self.validate_field::<CacheTtl>("system")?;
        self.validate_field::<CacheTtl>("tools")?;
        self.validate_field::<RollingCacheTtl>("rolling")?;
        decode_and_validate(&self.0, AnthropicCacheConfig::validate)
    }

    pub fn bedrock(&self) -> Result<BedrockCacheConfig, String> {
        self.validate_field::<CacheTtl>("system")?;
        self.validate_field::<CacheTtl>("tools")?;
        self.validate_field::<RollingCacheTtl>("rolling")?;
        decode_and_validate(&self.0, BedrockCacheConfig::validate)
    }

    pub fn openai(&self) -> Result<OpenAiCacheConfig, String> {
        self.validate_field::<OpenAiPromptCacheRetention>("prompt_cache_retention")?;
        self.validate_field::<OpenAiCacheMode>("mode")?;
        self.validate_field::<OpenAiPromptCacheTtl>("ttl")?;
        self.validate_field::<bool>("system")?;
        self.validate_field::<bool>("rolling")?;
        serde_json::from_value(self.0.clone()).map_err(|error| error.to_string())
    }

    pub fn openai_compatible(&self) -> Result<OpenAiCompatibleCacheConfig, String> {
        self.validate_field::<String>("prompt_cache_key")?;
        let config: OpenAiCompatibleCacheConfig =
            serde_json::from_value(self.0.clone()).map_err(|error| error.to_string())?;
        if let Some(key) = &config.prompt_cache_key {
            let expanded = key.replace("${session_id}", "00000000-0000-0000-0000-000000000000");
            if expanded.contains("${") || expanded.chars().count() > 64 {
                return Err(
                    "prompt_cache_key supports only ${session_id} and must expand to at most 64 characters"
                        .into(),
                );
            }
        }
        Ok(config)
    }

    fn validate_field<T>(&self, field: &str) -> Result<(), String>
    where
        T: serde::de::DeserializeOwned,
    {
        let Some(value) = self.0.get(field) else {
            return Ok(());
        };
        serde_json::from_value::<T>(value.clone())
            .map(|_| ())
            .map_err(|error| format!("field `{field}`: {error}"))
    }
}

fn decode_and_validate<T>(
    value: &serde_json::Value,
    validate: impl FnOnce(&T) -> Result<(), &'static str>,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let config: T = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    validate(&config).map_err(str::to_owned)?;
    Ok(config)
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

const fn five_minutes() -> CacheTtl {
    CacheTtl::FiveMinutes
}

const fn rolling_five_minutes() -> RollingCacheTtl {
    RollingCacheTtl::FiveMinutes
}
