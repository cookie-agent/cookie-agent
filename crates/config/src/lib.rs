//! Layered TOML configuration types and Figment loader skeleton.

use std::{collections::BTreeMap, path::Path};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProfile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentProfile {
    #[serde(rename = "type", default)]
    pub r#type: AgentType,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub delegation: DelegationConfig,
    #[serde(default)]
    pub permission_rules: Vec<PermissionRule>,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            r#type: AgentType::All,
            models: Vec::new(),
            tools: Vec::new(),
            delegation: DelegationConfig::default(),
            permission_rules: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    #[default]
    All,
    Primary,
    SubAgent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DelegationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_profiles: Vec<String>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermissionRule {
    pub id: String,
    pub action: String,
    pub resource: String,
    pub effect: String,
    #[serde(default)]
    pub hard: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration extraction failed: {0}")]
    Figment(#[source] Box<figment::Error>),
}

/// Builds the standard defaults < user < workspace < environment Figment stack.
#[must_use]
pub fn build_figment(user_config: Option<&Path>, workspace_config: Option<&Path>) -> Figment {
    let mut figment = Figment::from(Serialized::defaults(Config::default()));
    if let Some(path) = user_config {
        figment = figment.merge(Toml::file(path));
    }
    if let Some(path) = workspace_config {
        figment = figment.merge(Toml::file(path));
    }
    figment.merge(Env::prefixed("COOKIECODE_").split("__"))
}

pub fn load_from(figment: Figment) -> Result<Config, ConfigError> {
    figment
        .extract()
        .map_err(|error| ConfigError::Figment(Box::new(error)))
}

/// Concatenating layered permission rules needs custom Figment merge behavior.
pub fn merge_permission_rules(_layers: Vec<Vec<PermissionRule>>) -> Vec<PermissionRule> {
    todo!("implement ordered permission-rule concatenation")
}
