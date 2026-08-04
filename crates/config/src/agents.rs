use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{
    AgentId, ConfiguredVariantRef, ModelKey, ModelSelection, SafeCode, WildcardPattern,
};
use cookie_agent_models::{ModelSet, Sha256Digest};
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

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    pub id: AgentId,
    pub description: String,
    pub mode: AgentMode,
    pub enabled: bool,
    pub runnable_as_root: bool,
    pub resolved_fallback: Vec<ModelSelection>,
    pub tools: Vec<ToolName>,
    pub delegation_targets: Vec<AgentId>,
}

#[derive(Clone, Debug)]
pub struct ResolvedAgent {
    pub document: AgentDocument,
    pub resolved_fallback: Vec<ModelSelection>,
    pub runnable_as_root: bool,
    model_snapshot_fingerprint: Sha256Digest,
}

/// Exact executable fallback plan for a public root selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootModelPlan {
    selections: Vec<ModelSelection>,
}

impl RootModelPlan {
    #[must_use]
    pub fn selections(&self) -> &[ModelSelection] {
        &self.selections
    }

    #[must_use]
    pub fn into_selections(self) -> Vec<ModelSelection> {
        self.selections
    }
}

/// Existing chain-only suffix plan used for delegated agents with authored fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedModelPlan {
    selections: Vec<ModelSelection>,
}

impl DelegatedModelPlan {
    #[must_use]
    pub fn selections(&self) -> &[ModelSelection] {
        &self.selections
    }

    #[must_use]
    pub fn into_selections(self) -> Vec<ModelSelection> {
        self.selections
    }
}

impl ResolvedAgent {
    #[must_use]
    pub fn model_snapshot_fingerprint(&self) -> &Sha256Digest {
        &self.model_snapshot_fingerprint
    }

    /// Builds a root plan from one coherent model snapshot.
    pub fn plan_root_selection(
        &self,
        selection: &ModelSelection,
        models: &ModelSet,
    ) -> Result<RootModelPlan, ConfigError> {
        self.validate_model_snapshot(models)?;
        if !self.runnable_as_root {
            return Err(ConfigError::IneligibleRootAgent(self.document.id.clone()));
        }
        validate_selection(&self.document.id, selection, models)?;

        let authored_start = self
            .resolved_fallback
            .iter()
            .position(|entry| entry.model == selection.model);
        let authored = authored_start.map_or(self.resolved_fallback.as_slice(), |index| {
            &self.resolved_fallback[index..]
        });
        let mut selections = authored
            .iter()
            .filter(|entry| model_is_available(models, &entry.model))
            .cloned()
            .collect::<Vec<_>>();

        if authored_start.is_some() {
            selections[0] = selection.clone();
        } else {
            selections.insert(0, selection.clone());
        }
        Ok(RootModelPlan { selections })
    }

    /// Selects the unique authored suffix for delegated planning.
    pub fn plan_delegated_selection(
        &self,
        selection: &ModelSelection,
        models: &ModelSet,
    ) -> Result<DelegatedModelPlan, ConfigError> {
        self.validate_model_snapshot(models)?;
        let index = self
            .resolved_fallback
            .iter()
            .position(|entry| entry.model == selection.model)
            .ok_or_else(|| ConfigError::InvalidRunSelection {
                agent: self.document.id.clone(),
                model: selection.model.clone(),
            })?;
        validate_selection(&self.document.id, selection, models)?;
        let mut suffix = self.resolved_fallback[index..].to_vec();
        suffix[0] = selection.clone();
        Ok(DelegatedModelPlan { selections: suffix })
    }

    fn validate_model_snapshot(&self, models: &ModelSet) -> Result<(), ConfigError> {
        if self.model_snapshot_fingerprint == *models.fingerprint() {
            Ok(())
        } else {
            Err(ConfigError::ModelSnapshotMismatch(self.document.id.clone()))
        }
    }
}

fn validate_selection(
    agent: &AgentId,
    selection: &ModelSelection,
    models: &ModelSet,
) -> Result<(), ConfigError> {
    let model = models
        .get(&selection.model)
        .filter(|model| model.is_available())
        .ok_or_else(|| ConfigError::InvalidRunSelection {
            agent: agent.clone(),
            model: selection.model.clone(),
        })?;
    if let Some(variant) = &selection.variant
        && !model.variants().contains_key(variant)
    {
        return Err(ConfigError::UnknownVariant {
            agent: agent.clone(),
            model: selection.model.clone(),
            variant: variant.to_string(),
        });
    }
    Ok(())
}

fn model_is_available(models: &ModelSet, key: &ModelKey) -> bool {
    models
        .get(key)
        .is_some_and(cookie_agent_models::ModelEntry::is_available)
}

#[derive(Clone, Debug)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, ResolvedAgent>,
}

impl AgentRegistry {
    pub fn resolve(
        documents: BTreeMap<AgentId, AgentDocument>,
        models: &ModelSet,
    ) -> Result<Self, ConfigError> {
        for document in documents.values() {
            validate_agent_document(document, &documents)?;
        }
        let mut agents = BTreeMap::new();
        for (id, document) in documents {
            let mut resolved = Vec::with_capacity(document.frontmatter.model_fallback.len());
            let mut seen = BTreeSet::new();
            for fallback in &document.frontmatter.model_fallback {
                if !seen.insert(fallback.model.clone()) {
                    return Err(ConfigError::DuplicateFallbackModel {
                        agent: id.clone(),
                        model: fallback.model.clone(),
                    });
                }
                let entry =
                    models
                        .get(&fallback.model)
                        .ok_or_else(|| ConfigError::UnknownModel {
                            agent: id.clone(),
                            model: fallback.model.clone(),
                        })?;
                let variant = match &fallback.variant {
                    None => entry.default_variant().cloned(),
                    Some(ConfiguredVariantRef::Base) => None,
                    Some(ConfiguredVariantRef::Named(variant)) => {
                        if !entry.variants().contains_key(variant) {
                            return Err(ConfigError::UnknownVariant {
                                agent: id.clone(),
                                model: fallback.model.clone(),
                                variant: variant.to_string(),
                            });
                        }
                        Some(variant.clone())
                    }
                };
                resolved.push(ModelSelection {
                    model: fallback.model.clone(),
                    variant,
                });
            }
            let available = resolved
                .iter()
                .any(|selection| model_is_available(models, &selection.model));
            let runnable_as_root = document.frontmatter.enabled
                && matches!(
                    document.frontmatter.mode,
                    AgentMode::Primary | AgentMode::All
                )
                && !resolved.is_empty()
                && available;
            agents.insert(
                id,
                ResolvedAgent {
                    document,
                    resolved_fallback: resolved,
                    runnable_as_root,
                    model_snapshot_fingerprint: models.fingerprint().clone(),
                },
            );
        }
        Ok(Self { agents })
    }

    #[must_use]
    pub fn get(&self, id: &AgentId) -> Option<&ResolvedAgent> {
        self.agents.get(id)
    }
    pub fn agents(&self) -> impl ExactSizeIterator<Item = (&AgentId, &ResolvedAgent)> {
        self.agents.iter()
    }
    pub fn descriptors(&self) -> Vec<AgentDescriptor> {
        self.agents
            .iter()
            .map(|(id, agent)| AgentDescriptor {
                id: id.clone(),
                description: agent.document.frontmatter.description.clone(),
                mode: agent.document.frontmatter.mode,
                enabled: agent.document.frontmatter.enabled,
                runnable_as_root: agent.runnable_as_root,
                resolved_fallback: agent.resolved_fallback.clone(),
                tools: agent.document.frontmatter.tools.clone(),
                delegation_targets: agent
                    .document
                    .frontmatter
                    .delegation
                    .as_ref()
                    .map_or_else(Vec::new, |delegation| delegation.agents.clone()),
            })
            .collect()
    }
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
