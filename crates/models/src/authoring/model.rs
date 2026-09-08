use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{HeaderName, PartialRequestDefaults, SafeStaticHeaderValue};
use crate::{
    CompactionCapability, MediaCapability, MediaKind, Modality, ModelCapabilities, ProviderOptions,
    ReasoningBehavior, ReplayCapability, RequestDefaults, catalog::PicoUsdPerMillion,
};

/// Provider wire identity, deliberately independent of local model-key syntax.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WireModelId(String);

impl WireModelId {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        oven_sdk::ModelId::new(&value)
            .validate()
            .map_err(|_| "invalid wire model_id")?;
        if value.trim() != value || value.len() > 2048 {
            return Err(
                "model_id must be nonempty, at most 2048 bytes, and have no surrounding whitespace",
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WireModelId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestEndpoint {
    Completions,
    Responses,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptorOptions {
    pub request_endpoint: Option<RequestEndpoint>,
    pub beta: Option<Vec<String>>,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub store: Option<bool>,
}

impl AdaptorOptions {
    pub(crate) fn apply(&self, base: &mut ProviderOptions) {
        base.request_endpoint = self.request_endpoint.or(base.request_endpoint);
        if let Some(beta) = &self.beta {
            base.beta.clone_from(beta);
        }
        if let Some(value) = &self.organization {
            base.organization = Some(value.clone());
        }
        if let Some(value) = &self.project {
            base.project = Some(value.clone());
        }
        base.store = self.store.or(base.store);
    }

    pub(crate) fn resolve(&self) -> ProviderOptions {
        let mut result = ProviderOptions::default();
        self.apply(&mut result);
        result
    }
}

impl PartialRequestDefaults {
    pub(crate) fn apply(&self, base: &mut RequestDefaults) {
        base.temperature = self.temperature.or(base.temperature);
        base.top_p = self.top_p.or(base.top_p);
        base.max_output_tokens = self.max_output_tokens.or(base.max_output_tokens);
        if let Some(stop) = &self.stop {
            base.stop.clone_from(stop);
        }
        base.seed = self.seed.or(base.seed);
        if let Some(choice) = &self.tool_choice {
            base.tool_choice = Some(choice.clone());
        }
    }

    pub(crate) fn resolve(&self) -> RequestDefaults {
        let mut result = RequestDefaults::default();
        self.apply(&mut result);
        result
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VariantDefinition {
    pub enabled: Option<bool>,
    pub model_id: Option<WireModelId>,
    pub display_name: Option<String>,
    pub generation_options: Option<PartialRequestDefaults>,
    pub adaptor_options: Option<AdaptorOptions>,
    pub reasoning: Option<ReasoningBehavior>,
    #[serde(
        default,
        deserialize_with = "super::deserialize_optional_model_headers"
    )]
    pub headers: Option<BTreeMap<HeaderName, SafeStaticHeaderValue>>,
}

impl VariantDefinition {
    pub(crate) fn disabled_has_settings(&self) -> bool {
        self.model_id.is_some()
            || self.display_name.is_some()
            || self.generation_options.is_some()
            || self.adaptor_options.is_some()
            || self.reasoning.is_some()
            || self.headers.is_some()
    }

    pub fn headers(&self) -> &BTreeMap<HeaderName, SafeStaticHeaderValue> {
        static EMPTY: BTreeMap<HeaderName, SafeStaticHeaderValue> = BTreeMap::new();
        self.headers.as_ref().unwrap_or(&EMPTY)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredCapabilities {
    pub input: BTreeSet<Modality>,
    pub output: BTreeSet<Modality>,
    pub context_tokens: u64,
    pub output_tokens: u64,
    #[serde(default = "super::yes")]
    pub tool_calling: bool,
    #[serde(default = "super::yes")]
    pub parallel_tool_calls: bool,
    #[serde(default = "super::yes")]
    pub structured_output: bool,
    pub reasoning: bool,
    pub temperature: bool,
    pub top_p: bool,
    pub seed: bool,
    #[serde(default)]
    pub compaction: CompactionCapability,
    pub native_replay: Option<ReplayCapability>,
    pub media: BTreeMap<MediaKind, MediaCapability>,
}

impl AuthoredCapabilities {
    pub(crate) fn resolve(&self, adapter: crate::adapters::OvenAdapterFamily) -> ModelCapabilities {
        ModelCapabilities {
            input: self.input.clone(),
            output: self.output.clone(),
            context_tokens: self.context_tokens,
            output_tokens: self.output_tokens,
            tool_calling: self.tool_calling,
            parallel_tool_calls: self.parallel_tool_calls,
            structured_output: self.structured_output,
            reasoning: self.reasoning,
            temperature: self.temperature,
            top_p: self.top_p,
            seed: self.seed,
            compaction: self.compaction,
            native_replay: self
                .native_replay
                .unwrap_or_else(|| adapter.automatic_replay(self.reasoning)),
            cancellation: crate::CancellationCapability::LocalOnly,
            media: self.media.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_per_million_usd: Option<PicoUsdPerMillion>,
    pub output_per_million_usd: Option<PicoUsdPerMillion>,
    pub reasoning_per_million_usd: Option<PicoUsdPerMillion>,
    pub cache_read_per_million_usd: Option<PicoUsdPerMillion>,
    pub cache_write_per_million_usd: Option<PicoUsdPerMillion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_options_preserve_absence_and_apply_false_and_empty_lists() {
        let mut options = ProviderOptions {
            store: Some(true),
            beta: vec!["inherited".into()],
            organization: Some("org".into()),
            ..ProviderOptions::default()
        };
        let empty: AdaptorOptions = toml::from_str("").unwrap();
        empty.apply(&mut options);
        assert_eq!(options.store, Some(true));
        assert_eq!(options.beta, ["inherited"]);
        let overlay: AdaptorOptions = toml::from_str("store = false\nbeta = []").unwrap();
        overlay.apply(&mut options);
        assert_eq!(options.store, Some(false));
        assert!(options.beta.is_empty());
        assert_eq!(options.organization.as_deref(), Some("org"));
    }
}
