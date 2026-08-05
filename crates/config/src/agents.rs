use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{AgentId, ConfiguredVariantRef, ModelKey, SafeCode, WildcardPattern};
use serde::{Deserialize, Serialize};

use crate::{AgentDocument, ConfigError};

const AGENT_SCHEMA: u32 = 1;
const MAX_LIST: usize = 256;

/// Exact schema-1 agent marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AgentSchemaVersion;

impl<'de> Deserialize<'de> for AgentSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if value == AGENT_SCHEMA {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("agent schema must be exactly 1"))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Primary,
    Subagent,
    All,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolName {
    Read,
    Write,
    Edit,
    Bash,
    Grep,
    Glob,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Read,
    Write,
    Bash,
    Grep,
    Glob,
    Delegate,
    ExternalDirectory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    pub id: SafeCode,
    pub action: PermissionAction,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDelegationConfig {
    pub agents: Vec<AgentId>,
    pub max_depth: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelFallback {
    pub model: ModelKey,
    pub variant: Option<ConfiguredVariantRef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFrontmatter {
    pub schema: AgentSchemaVersion,
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub model_fallback: Vec<AgentModelFallback>,
    pub tools: Vec<ToolName>,
    pub permissions: Vec<PermissionRule>,
    pub delegation: Option<AgentDelegationConfig>,
}

/// Validated authored-agent input for downstream runtime materialization.
///
/// `root_eligible` covers only authored policy. P7 computes `runnable_as_root`
/// by additionally requiring at least one available materialized fallback.
#[derive(Clone, Copy, Debug)]
pub struct AgentMaterializationInput<'a> {
    pub document: &'a AgentDocument,
    pub root_eligible: bool,
}

/// Atomically validated authored agent documents, independent of model execution.
#[derive(Clone, Debug)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentDocument>,
}

impl AgentRegistry {
    pub fn validate(agents: BTreeMap<AgentId, AgentDocument>) -> Result<Self, ConfigError> {
        Self::validate_ref(&agents)?;
        Ok(Self { agents })
    }

    pub(crate) fn validate_ref(
        agents: &BTreeMap<AgentId, AgentDocument>,
    ) -> Result<(), ConfigError> {
        for document in agents.values() {
            validate_agent_document(document, agents)?;
        }
        Ok(())
    }

    pub(crate) fn from_validated(agents: BTreeMap<AgentId, AgentDocument>) -> Self {
        Self { agents }
    }

    #[must_use]
    pub fn get(&self, id: &AgentId) -> Option<&AgentDocument> {
        self.agents.get(id)
    }

    pub fn agents(&self) -> impl ExactSizeIterator<Item = (&AgentId, &AgentDocument)> {
        self.agents.iter()
    }

    #[must_use]
    pub fn documents(&self) -> &BTreeMap<AgentId, AgentDocument> {
        &self.agents
    }

    #[must_use]
    pub fn into_documents(self) -> BTreeMap<AgentId, AgentDocument> {
        self.agents
    }

    pub fn materialization_inputs(
        &self,
    ) -> impl ExactSizeIterator<Item = AgentMaterializationInput<'_>> {
        self.agents
            .values()
            .map(|document| AgentMaterializationInput {
                document,
                root_eligible: authored_root_eligible(document),
            })
    }
}

fn authored_root_eligible(document: &AgentDocument) -> bool {
    document.frontmatter.enabled
        && matches!(
            document.frontmatter.mode,
            AgentMode::Primary | AgentMode::All
        )
        && !document.frontmatter.model_fallback.is_empty()
}

fn validate_agent_document(
    document: &AgentDocument,
    all: &BTreeMap<AgentId, AgentDocument>,
) -> Result<(), ConfigError> {
    let frontmatter = &document.frontmatter;
    if frontmatter.description.is_empty()
        || frontmatter.description.len() > 512
        || frontmatter.description.chars().any(char::is_control)
    {
        return Err(ConfigError::AgentField {
            agent: document.id.clone(),
            field: "description",
        });
    }
    if matches!(frontmatter.mode, AgentMode::Primary) && frontmatter.model_fallback.is_empty() {
        return Err(ConfigError::PrimaryFallback(document.id.clone()));
    }
    for length in [
        frontmatter.model_fallback.len(),
        frontmatter.tools.len(),
        frontmatter.permissions.len(),
    ] {
        if length > MAX_LIST {
            return Err(ConfigError::AgentLimit(document.id.clone()));
        }
    }
    let mut fallback_models = BTreeSet::new();
    for fallback in &frontmatter.model_fallback {
        if !fallback_models.insert(fallback.model.clone()) {
            return Err(ConfigError::DuplicateFallbackModel {
                agent: document.id.clone(),
                model: fallback.model.clone(),
            });
        }
    }
    if frontmatter.tools.iter().collect::<BTreeSet<_>>().len() != frontmatter.tools.len() {
        return Err(ConfigError::DuplicateTool(document.id.clone()));
    }
    let mut rules = BTreeSet::new();
    for rule in &frontmatter.permissions {
        if !rules.insert(rule.id.clone()) {
            return Err(ConfigError::PermissionRule(document.id.clone()));
        }
    }
    if let Some(delegation) = &frontmatter.delegation {
        if delegation.agents.is_empty()
            || delegation.agents.len() > MAX_LIST
            || delegation.agents.iter().collect::<BTreeSet<_>>().len() != delegation.agents.len()
        {
            return Err(ConfigError::Delegation(document.id.clone()));
        }
        for target in &delegation.agents {
            let target_document =
                all.get(target)
                    .ok_or_else(|| ConfigError::UnknownDelegationTarget {
                        agent: document.id.clone(),
                        target: target.clone(),
                    })?;
            if !target_document.frontmatter.enabled
                || !matches!(
                    target_document.frontmatter.mode,
                    AgentMode::Subagent | AgentMode::All
                )
            {
                return Err(ConfigError::IneligibleDelegationTarget {
                    agent: document.id.clone(),
                    target: target.clone(),
                });
            }
        }
    }
    Ok(())
}
