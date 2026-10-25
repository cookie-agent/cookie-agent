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

/// The layer of the session tree a run belongs to. Structural cache defaults
/// differ by layer; authored values apply to both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheLayer {
    /// A session with no delegation parent.
    Root,
    /// A delegated child session (a subagent).
    Delegated,
}

/// The TTLs of the three structural cache markers, in marker order: tools,
/// system, then the rolling marker on the last non-system message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheTtls {
    pub tools: CacheTtl,
    pub system: CacheTtl,
    pub rolling: CacheTtl,
}

impl CacheTtls {
    /// The defaults of `layer`: one-hour system and tools markers, and a
    /// one-hour rolling marker for root sessions but a five-minute one for
    /// delegated sessions. Without `one_hour` (a model that cannot take a
    /// one-hour TTL) every marker defaults to five minutes.
    #[must_use]
    pub const fn defaults(layer: CacheLayer, one_hour: bool) -> Self {
        let long = if one_hour {
            CacheTtl::OneHour
        } else {
            CacheTtl::FiveMinutes
        };
        Self {
            tools: long,
            system: long,
            rolling: match layer {
                CacheLayer::Root => long,
                CacheLayer::Delegated => CacheTtl::FiveMinutes,
            },
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        validate_ttl_order([self.tools, self.system, self.rolling].map(ttl))
    }
}

/// The structural cache table of Anthropic and Bedrock providers: one TTL
/// per marker. An unset field takes the default of the run's
/// [`CacheLayer`] ([`Self::resolve`]).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralCacheConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<CacheTtl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<CacheTtl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling: Option<CacheTtl>,
}

pub type AnthropicCacheConfig = StructuralCacheConfig;
pub type BedrockCacheConfig = StructuralCacheConfig;

impl StructuralCacheConfig {
    /// Checks the marker order of the authored values: a one-hour marker
    /// cannot follow a five-minute one. A default never puts a longer
    /// marker after a shorter one ([`Self::resolve`]); an authored one-hour
    /// marker after five-minute defaults (a Bedrock model without one-hour
    /// support) only shows once resolved ([`CacheTtls::validate`]).
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_ttl_order([self.tools, self.system, self.rolling].map(|value| value.and_then(ttl)))
    }

    /// The TTLs this table selects over `defaults`. An authored value wins;
    /// a default one-hour TTL that would follow a five-minute marker drops
    /// to five minutes, so a default never breaks the marker order.
    #[must_use]
    pub fn resolve(&self, defaults: CacheTtls) -> CacheTtls {
        let mut saw_five_minutes = false;
        let mut pick = |authored: Option<CacheTtl>, default: CacheTtl| {
            let value = match authored {
                Some(value) => value,
                None if saw_five_minutes && default == CacheTtl::OneHour => CacheTtl::FiveMinutes,
                None => default,
            };
            saw_five_minutes |= value == CacheTtl::FiveMinutes;
            value
        };
        let tools = pick(self.tools, defaults.tools);
        let system = pick(self.system, defaults.system);
        let rolling = pick(self.rolling, defaults.rolling);
        CacheTtls {
            tools,
            system,
            rolling,
        }
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
        self.structural()
    }

    pub fn bedrock(&self) -> Result<BedrockCacheConfig, String> {
        self.structural()
    }

    fn structural(&self) -> Result<StructuralCacheConfig, String> {
        self.validate_field::<CacheTtl>("system")?;
        self.validate_field::<CacheTtl>("tools")?;
        self.validate_field::<CacheTtl>("rolling")?;
        decode_and_validate(&self.0, StructuralCacheConfig::validate)
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

#[cfg(test)]
mod tests {
    use super::{CacheLayer, CacheTtl, CacheTtls, StructuralCacheConfig};

    const HOUR: CacheTtl = CacheTtl::OneHour;
    const FIVE: CacheTtl = CacheTtl::FiveMinutes;
    const OFF: CacheTtl = CacheTtl::Off;

    fn ttls(tools: CacheTtl, system: CacheTtl, rolling: CacheTtl) -> CacheTtls {
        CacheTtls {
            tools,
            system,
            rolling,
        }
    }

    #[test]
    fn layer_defaults_differ_only_in_the_rolling_marker() {
        let root = CacheTtls::defaults(CacheLayer::Root, true);
        let delegated = CacheTtls::defaults(CacheLayer::Delegated, true);
        assert_eq!(root.rolling, HOUR);
        assert_eq!(delegated.rolling, FIVE);
        assert_eq!(
            (root.tools, root.system),
            (delegated.tools, delegated.system)
        );
        for layer in [CacheLayer::Root, CacheLayer::Delegated] {
            assert_eq!(CacheTtls::defaults(layer, false), ttls(FIVE, FIVE, FIVE));
        }
    }

    #[test]
    fn authored_values_win_over_the_defaults_of_either_layer() {
        let config = StructuralCacheConfig {
            system: None,
            tools: Some(OFF),
            rolling: Some(HOUR),
        };
        for defaults in [ttls(HOUR, HOUR, HOUR), ttls(HOUR, HOUR, FIVE)] {
            assert_eq!(config.resolve(defaults), ttls(OFF, HOUR, HOUR));
        }
    }

    #[test]
    fn a_default_never_follows_a_shorter_marker_with_a_longer_one() {
        let config = StructuralCacheConfig {
            system: Some(FIVE),
            tools: None,
            rolling: None,
        };
        let resolved = config.resolve(ttls(HOUR, HOUR, HOUR));
        assert_eq!(resolved, ttls(HOUR, FIVE, FIVE));
        assert!(resolved.validate().is_ok());

        let off_between = StructuralCacheConfig {
            system: Some(OFF),
            tools: Some(FIVE),
            rolling: None,
        };
        assert_eq!(
            off_between.resolve(ttls(HOUR, HOUR, HOUR)),
            ttls(FIVE, OFF, FIVE)
        );
    }

    #[test]
    fn an_authored_one_hour_marker_after_five_minute_defaults_is_out_of_order() {
        let config = StructuralCacheConfig {
            system: None,
            tools: None,
            rolling: Some(HOUR),
        };
        assert!(config.validate().is_ok());
        assert!(config.resolve(ttls(FIVE, FIVE, FIVE)).validate().is_err());
    }

    #[test]
    fn authored_marker_order_ignores_off_markers() {
        let valid = StructuralCacheConfig {
            system: Some(OFF),
            tools: Some(HOUR),
            rolling: Some(FIVE),
        };
        assert!(valid.validate().is_ok());
        let invalid = StructuralCacheConfig {
            system: Some(OFF),
            tools: Some(FIVE),
            rolling: Some(HOUR),
        };
        assert!(invalid.validate().is_err());
    }
}
